//! DFM+ prepare 调度（自有层，M2）：字体与测量、布局参数、请求组装、后台 worker。
//!
//! TDD 6.5 契约：
//! - 测量与绘制同源（ab_glyph 同一 FontArc 与 PxScale，碰撞一致性）
//! - 重 prepare 在独立 worker 执行（不阻塞播放与渲染），期间沿用旧 layout，
//!   完成后原子替换并 generation+1——参数调节无闪烁、无黑帧
//! - 滚动时长经 vendor factory 公式由视口宽度与速度系数换算

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use std::thread::JoinHandle;

use ab_glyph::{Font, FontArc, GlyphId, ScaleFont};

use crate::dfm::{PrepareItem, PrepareRequest, PreparedLayout};
use crate::normalize::{NormalizedItem, SpecialData, TopBottomConvert, remap_top_bottom};
use crate::typography;

/// Windows 系统字体候选链（与 dyplayer-app 的 CJK 字体安装路径一致，
/// 内嵌字体子集为后续里程碑项）。
const SYSTEM_FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\msyh.ttf",
    r"C:\Windows\Fonts\simhei.ttf",
    r"C:\Windows\Fonts\simsun.ttc",
];

/// 弹幕字体：prepare 测量与 atlas 栅格化共用同一 FontArc（同源契约）。
#[derive(Debug, Clone)]
pub struct DanmakuFont {
    font: FontArc,
}

impl DanmakuFont {
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        FontArc::try_from_vec(bytes).ok().map(|font| Self { font })
    }

    /// 按候选路径依次加载（任一可读且可解析即命中；全 miss 返回 None）。
    /// 供宿主按用户字体选择组装候选链（选择 + 交叉回退）。
    pub fn from_paths(paths: &[PathBuf]) -> Option<Self> {
        for path in paths {
            if let Ok(bytes) = std::fs::read(path)
                && let Some(font) = Self::from_bytes(bytes)
            {
                return Some(font);
            }
        }
        None
    }

    /// 按候选链加载系统字体；全部失败返回 None（弹幕不可用，UI 需提示）。
    pub fn from_system() -> Option<Self> {
        for path in SYSTEM_FONT_CANDIDATES {
            if let Ok(bytes) = std::fs::read(path)
                && let Some(font) = Self::from_bytes(bytes)
            {
                return Some(font);
            }
        }
        None
    }

    pub fn font(&self) -> &FontArc {
        &self.font
    }

    /// 测量文本：返回 `(宽度, 行高)`（物理像素）。
    ///
    /// 宽度为逐字形 advance 累加（含 kerning），不含描边外扩（dfm.rs 组装时
    /// 按 outline profile 自行加宽，上游契约）；行高为 ascent - descent。
    pub(crate) fn measure(&self, text: &str, px_per_em: f32) -> (f64, f64) {
        let scale = typography::px_scale_for_em(&self.font, px_per_em);
        let scaled = self.font.as_scaled(scale);
        let line_height = scaled.ascent() - scaled.descent();
        let mut max_width = 0.0f32;
        let mut line_count = 0u32;
        for line in text.split('\n') {
            let mut width = 0.0f32;
            let mut prev: Option<GlyphId> = None;
            for ch in line.chars() {
                let id = self.font.glyph_id(ch);
                if let Some(prev_id) = prev {
                    width += scaled.kern(prev_id, id);
                }
                width += scaled.h_advance(id);
                prev = Some(id);
            }
            max_width = max_width.max(width);
            line_count += 1;
        }
        let height = line_height * line_count as f32;
        (f64::from(max_width.max(0.0)), f64::from(height.max(0.0)))
    }
}

/// 触发后台重 prepare 的布局参数集合（TDD 6.5 设置表"异步重 prepare"项）。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutParams {
    /// 视频显示区域（物理像素）；尺寸变化触发重 prepare。
    pub viewport_width: f64,
    pub viewport_height: f64,
    /// 用户配置基准字号（B站源字号 25 相对缩放的基准）。
    pub base_font_size: f32,
    /// 弹幕显示区域占比（0.1 - 1.0）。
    pub display_area: f64,
    /// 滚动速度系数（经 vendor factory 公式换算滚动时长）。
    pub scroll_speed_factor: f64,
    /// 轨道行距比（0.0 - 2.0）。
    pub track_gap_ratio: f64,
    /// 描边 profile（0=off 1=fine 2=normal 3=thick）。
    pub outline_profile: f64,
    /// 重复弹幕合并（x N）。
    pub merge_danmaku: bool,
    /// 密度上限（同屏数量）。
    pub max_quantity: Option<u32>,
    /// 每类弹幕最大轨道行数。
    pub max_lines_per_type: Option<u32>,
    /// 屏蔽词列表。
    pub block_words: Vec<String>,
    /// 类型开关。
    pub block_top: bool,
    pub block_bottom: bool,
    pub block_scroll: bool,
    /// 顶部/底部弹幕转换（app 经 Default 保持 Keep；面板接线预留）。
    pub top_bottom_convert: TopBottomConvert,
    /// 允许重叠堆叠。
    pub allow_stacking: bool,
    /// 允许滚动弹幕占用轨道溢出项。
    pub allow_scroll_overwrite: bool,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            viewport_width: 1280.0,
            viewport_height: 720.0,
            base_font_size: typography::DEFAULT_CONFIG_FONT_SIZE,
            display_area: 1.0,
            scroll_speed_factor: 1.0,
            track_gap_ratio: 0.0,
            outline_profile: 2.0,
            merge_danmaku: false,
            max_quantity: None,
            max_lines_per_type: None,
            block_words: Vec::new(),
            block_top: false,
            block_bottom: false,
            block_scroll: false,
            top_bottom_convert: TopBottomConvert::default(),
            allow_stacking: false,
            allow_scroll_overwrite: false,
        }
    }
}

/// 顶底转换重映射后的类型码（纯映射，供组装与测试）。
pub(crate) fn resolved_type_code(item: &NormalizedItem, params: &LayoutParams) -> i32 {
    remap_top_bottom(item.type_code, params.top_bottom_convert)
}

/// B站源字号按基准缩放后的最终像素字号（纯映射，供组装与测试）。
pub(crate) fn resolved_font_size(item: &NormalizedItem, params: &LayoutParams) -> f32 {
    typography::effective_font_size(item.source_font_size, params.base_font_size, 1.0)
}

/// 组装 DFM+ prepare 输入：规范化条目 -> 测量后的 PrepareItem。
pub(crate) fn assemble_prepare_items(
    items: &[NormalizedItem],
    params: &LayoutParams,
    font: &DanmakuFont,
) -> Vec<PrepareItem> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let font_size = resolved_font_size(item, params);
            let (paint_width, paint_height) = font.measure(&item.text, font_size);
            PrepareItem {
                source_id: index as u64,
                time_seconds: item.time_seconds,
                text: item.text.clone(),
                type_code: resolved_type_code(item, params),
                color_argb: item.color_argb,
                opacity: 1.0,
                is_me: false,
                font_size,
                paint_width,
                paint_height,
                style: item.style.clone(),
                special: item
                    .special
                    .as_ref()
                    .map(|s| scale_special_to_viewport(s, params.viewport_width, params.viewport_height)),
            }
        })
        .collect()
}

/// mode7 历史播放器基准尺寸（B站老播放器 545×388，新 862×568）；
/// 2013-11-10 之前的老录播用旧基准（对照实现 GraphicPrivate.cpp:41-44）。
const HISTORICAL_BASE_OLD: (f64, f64) = (545.0, 388.0);
const HISTORICAL_BASE_NEW: (f64, f64) = (862.0, 568.0);
const HISTORICAL_CUTOFF_DATE: i64 = 1384099200;

/// 将 mode7 源坐标按历史基准还原到当前视口尺寸。
fn scale_special_to_viewport(s: &SpecialData, vw: f64, vh: f64) -> SpecialData {
    let base = if s.date < HISTORICAL_CUTOFF_DATE {
        HISTORICAL_BASE_OLD
    } else {
        HISTORICAL_BASE_NEW
    };
    let sx = vw / base.0;
    let sy = vh / base.1;
    SpecialData {
        x1: s.x1 * sx,
        y1: s.y1 * sy,
        x2: s.x2 * sx,
        y2: s.y2 * sy,
        alpha_from: s.alpha_from,
        alpha_to: s.alpha_to,
        duration_s: s.duration_s,
        z_rotate: s.z_rotate,
        date: s.date,
    }
}

/// 同步执行一次 DFM+ prepare（worker 线程内调用；万级弹幕目标 < 50ms）。
pub(crate) fn run_prepare(
    items: &[NormalizedItem],
    params: &LayoutParams,
    font: &DanmakuFont,
) -> Result<PreparedLayout, String> {
    let prepare_items = assemble_prepare_items(items, params, font);
    // UI 的 scroll_speed_factor 是速度语义（越大越快，常见弹幕引擎同义：
    // speed = 125 + width/5 px/s）；vendor factory 的因子是时长语义
    // （越大越慢），故取倒数换算，避免"调快反而更慢"。
    let duration_factor = 1.0 / params.scroll_speed_factor.max(0.1);
    let scroll_duration_seconds = crate::dfm_core::factory::compute_scroll_duration(
        params.viewport_width.max(1.0) as f32,
        duration_factor as f32,
    ) as f64
        / 1000.0;
    let preferred_tracks = HashMap::new();
    let request = PrepareRequest {
        items: prepare_items,
        width: params.viewport_width.max(1.0),
        height: params.viewport_height.max(1.0),
        font_size: f64::from(params.base_font_size.max(1.0)),
        display_area: params.display_area.clamp(0.1, 1.0),
        scroll_duration_seconds: scroll_duration_seconds.max(1.0),
        allow_stacking: params.allow_stacking,
        allow_scroll_overwrite: params.allow_scroll_overwrite,
        merge_danmaku: params.merge_danmaku,
        max_quantity: params.max_quantity,
        max_lines_per_type: params.max_lines_per_type,
        track_gap_ratio: params.track_gap_ratio.clamp(0.0, 2.0),
        outline_width: params.outline_profile.max(0.0),
        preferred_tracks: &preferred_tracks,
        block_words: params.block_words.clone(),
        block_top: params.block_top,
        block_bottom: params.block_bottom,
        block_scroll: params.block_scroll,
    };
    crate::dfm::prepare_layout(request)
}

/// worker 任务（一次后台 prepare）。
struct WorkerTask {
    generation: u64,
    items: Arc<Vec<NormalizedItem>>,
    params: LayoutParams,
    font: Arc<DanmakuFont>,
}

/// worker 结果（携带任务时的 generation，供 session 侧过期判定）。
pub(crate) struct WorkerResult {
    pub generation: u64,
    pub layout: Result<PreparedLayout, String>,
}

/// 后台 prepare worker：单线程串行执行。
///
/// Receiver 由 worker 线程独占（无锁），submit 仅入队；worker 取到任务后
/// 先排空队列只保留最新（过时任务被丢弃，不执行无谓 prepare）。正在执行
/// 的任务无法中断，其结果由 session 侧 generation 校验丢弃。
///
/// 历史：曾在 submit 侧持 Receiver 的 Mutex 排空——worker 空闲时持锁阻塞
/// 在 recv，submit 抢锁入队形成死锁（UI 线程冻结），已改为 worker 侧聚合。
pub(crate) struct LayoutWorker {
    task_tx: std::sync::mpsc::Sender<WorkerTask>,
    result_rx: std::sync::mpsc::Receiver<WorkerResult>,
    /// R12：保留 JoinHandle 确保生命周期与 LayoutWorker 一致（Drop 时
    /// 自然 detach，不 join 避免换媒体冻结 UI）。
    #[allow(dead_code)]
    handle: Option<JoinHandle<()>>,
}

impl LayoutWorker {
    pub fn spawn() -> Self {
        let (task_tx, task_rx) = std::sync::mpsc::channel::<WorkerTask>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<WorkerResult>();
        let handle = std::thread::Builder::new()
            .name("danmaku-prepare".into())
            .spawn(move || {
                let task_rx = task_rx;
                loop {
                    let Ok(mut task) = task_rx.recv() else {
                        break;
                    };
                    // 聚合：跳过排队中的过时任务，只执行最新
                    while let Ok(newer) = task_rx.try_recv() {
                        task = newer;
                    }
                    let layout = run_prepare(&task.items, &task.params, &task.font);
                    let result = WorkerResult {
                        generation: task.generation,
                        layout,
                    };
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn danmaku-prepare worker");
        Self {
            task_tx,
            result_rx,
            handle: Some(handle),
        }
    }

    /// 提交任务（worker 侧聚合丢弃过时任务，见类型文档）。
    pub(crate) fn submit(
        &self,
        generation: u64,
        items: Arc<Vec<NormalizedItem>>,
        params: LayoutParams,
        font: Arc<DanmakuFont>,
    ) {
        let _ = self.task_tx.send(WorkerTask {
            generation,
            items,
            params,
            font,
        });
    }

    /// 非阻塞收取已完成结果（帧循环 poll 用）。
    pub(crate) fn try_recv_result(&self) -> Option<WorkerResult> {
        self.result_rx.try_recv().ok()
    }
}

impl Drop for LayoutWorker {
    fn drop(&mut self) {
        // R12：把 task_tx 换成已关闭的 placeholder（原 sender drop 关闭通道），
        // worker 的 recv 返回 Err 后退出循环。不 join——detach 让 worker 在
        // 后台完成当前 prepare 后自然退出，避免换媒体时 UI 冻结。
        // Arc<DanmakuFont>/Arc<items> 引用计数保证 worker 持有资源存活到退出。
        // F15（可选）：保持 detach 现状——dyplayer-danmaku 无 tracing 依赖，
        // 且 worker 单任务后即退出，累积风险低（方案复核结论）。
        let (placeholder_tx, _placeholder_rx) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.task_tx, placeholder_tx));
        // handle 在 Drop 结束时自然 detach（JoinHandle::drop 不 join）。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::{DyGradientTarget, DyStyle, parse_danmaku_xml};

    fn system_font() -> Option<DanmakuFont> {
        DanmakuFont::from_system()
    }

    #[test]
    fn default_params_are_within_contract_bounds() {
        let params = LayoutParams::default();
        assert!(params.viewport_width > 0.0);
        assert!(params.viewport_height > 0.0);
        assert!((0.1..=1.0).contains(&params.display_area));
        assert!(params.base_font_size > 0.0);
        assert!(params.outline_profile >= 0.0);
    }

    #[test]
    fn resolved_font_size_scales_from_bili_base() {
        let params = LayoutParams {
            base_font_size: 30.0,
            ..LayoutParams::default()
        };
        let standard = NormalizedItem {
            time_seconds: 0.0,
            text: "a".into(),
            type_code: 1,
            color_argb: 0,
            source_font_size: 25.0,
            style: None,
            special: None,
        };
        let small = NormalizedItem {
            source_font_size: 18.0,
            ..standard.clone()
        };
        let large = NormalizedItem {
            source_font_size: 36.0,
            ..standard.clone()
        };
        assert_eq!(resolved_font_size(&standard, &params), 30.0);
        assert!((resolved_font_size(&small, &params) - 21.6).abs() < 1e-4);
        assert!((resolved_font_size(&large, &params) - 43.2).abs() < 1e-4);
    }

    #[test]
    fn resolved_type_code_applies_convert() {
        let mut params = LayoutParams::default();
        let top = NormalizedItem {
            time_seconds: 0.0,
            text: "a".into(),
            type_code: 5,
            color_argb: 0,
            source_font_size: 25.0,
            style: None,
            special: None,
        };
        assert_eq!(resolved_type_code(&top, &params), 5);
        params.top_bottom_convert = TopBottomConvert::ToScroll;
        assert_eq!(resolved_type_code(&top, &params), 1);
        params.top_bottom_convert = TopBottomConvert::Swap;
        assert_eq!(resolved_type_code(&top, &params), 4);
    }

    #[test]
    fn run_prepare_empty_items_yields_layout() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let layout = run_prepare(&[], &LayoutParams::default(), &font).expect("prepare");
        assert!(layout.items.is_empty());
        assert!(layout.track_count >= 1);
    }

    #[test]
    fn run_prepare_maps_items_through_dfm() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let xml = concat!(
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0">第一条</d>"#,
            r#"<d p="0.0,4,25,16777215,0,0,0,0,0">底部</d>"#,
            r#"<d p="0.0,5,25,16777215,0,0,0,0,0">顶部</d>"#,
        );
        let items = parse_danmaku_xml(xml);
        assert_eq!(items.len(), 3);
        let layout = run_prepare(&items, &LayoutParams::default(), &font).expect("prepare");
        assert_eq!(layout.items.len(), 3);
        let codes: Vec<i32> = layout.items.iter().map(|i| i.type_code).collect();
        assert!(codes.contains(&1));
        assert!(codes.contains(&4));
        assert!(codes.contains(&5));
        for item in &layout.items {
            assert!(item.y_position >= 0.0);
            assert!(item.font_size > 0.0);
            assert!(item.width > 0.0);
            assert!(item.height > 0.0);
            assert!(item.style.is_none());
        }
    }

    #[test]
    fn run_prepare_passes_dy_style_through() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        // dy 扩展样式经 normalize → PrepareItem → PreparedItem 全程透传
        // （不参与布局碰撞，仅渲染层消费）。
        let xml = concat!(
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0" s="15880350~3181517,0">描边渐变</d>"#,
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0" s="957226~955309,1">文本渐变</d>"#,
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0">普通</d>"#,
        );
        let items = parse_danmaku_xml(xml);
        assert_eq!(items.len(), 3);
        let layout = run_prepare(&items, &LayoutParams::default(), &font).expect("prepare");
        assert_eq!(layout.items.len(), 3);
        let by_text = |t: &str| layout.items.iter().find(|i| i.text == t).unwrap();
        let outline_item = by_text("描边渐变");
        assert_eq!(
            outline_item.style,
            Some(DyStyle {
                nodes: vec![15880350, 3181517],
                target: DyGradientTarget::Outline,
            })
        );
        let fill_item = by_text("文本渐变");
        assert_eq!(
            fill_item.style,
            Some(DyStyle {
                nodes: vec![957226, 955309],
                target: DyGradientTarget::Fill,
            })
        );
        assert!(by_text("普通").style.is_none());
    }

    #[test]
    fn run_prepare_merge_keeps_different_styles_separate() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        // 同文本不同渐变：merge_danmaku 开启时不合并（各自保留观感）；
        // 同文本同渐变：合并 + x2 计数。
        let xml = concat!(
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0" s="15880350~3181517,0">哈哈哈</d>"#,
            r#"<d p="0.1,1,25,16777215,0,0,0,0,0" s="957226~955309,0">哈哈哈</d>"#,
            r#"<d p="0.2,1,25,16777215,0,0,0,0,0" s="15880350~3181517,0">哈哈哈</d>"#,
        );
        let items = parse_danmaku_xml(xml);
        let params = LayoutParams {
            merge_danmaku: true,
            ..LayoutParams::default()
        };
        let layout = run_prepare(&items, &params, &font).expect("prepare");
        let styled: Vec<_> = layout
            .items
            .iter()
            .filter(|i| i.text.starts_with("哈哈哈"))
            .collect();
        assert_eq!(
            styled.len(),
            2,
            "同文本不同渐变不合并、同文本同渐变合并，应为 2 条"
        );
        let merged = styled
            .iter()
            .find(|i| i.duplicate_count == 2)
            .expect("同样式两条应合并为 x2");
        assert!(merged.text.contains("x2"));
    }

    #[test]
    fn run_prepare_ten_thousand_items_succeeds() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut xml = String::from("<i>");
        for i in 0..10_000 {
            xml.push_str(&format!(
                r#"<d p="{:.3},1,25,16777215,0,0,0,0,0">弹幕{i}号</d>"#,
                i as f64 / 100.0
            ));
        }
        xml.push_str("</i>");
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 10_000);
        let start = std::time::Instant::now();
        let layout = run_prepare(&items, &LayoutParams::default(), &font).expect("prepare");
        let elapsed = start.elapsed();
        eprintln!("10k prepare took {elapsed:?}");
        assert!(!layout.items.is_empty());
    }

    #[test]
    fn worker_completes_and_reports_generation() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let worker = LayoutWorker::spawn();
        let items = Arc::new(parse_danmaku_xml(
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0">worker 弹幕</d>"#,
        ));
        worker.submit(7, items, LayoutParams::default(), Arc::new(font));
        let mut result = None;
        for _ in 0..200 {
            if let Some(r) = worker.try_recv_result() {
                result = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let result = result.expect("worker result");
        assert_eq!(result.generation, 7);
        assert!(result.layout.is_ok());
    }

    /// 防回归：worker 空闲（已在 recv 等待）时 submit 不得死锁。
    /// 旧实现的 submit 侧持 Receiver Mutex 排空，与持锁 recv 的 worker 互锁。
    #[test]
    fn submit_while_worker_idle_does_not_deadlock() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let worker = LayoutWorker::spawn();
        // 等 worker 进入空闲等待（recv 阻塞）
        std::thread::sleep(std::time::Duration::from_millis(50));
        let items = Arc::new(parse_danmaku_xml(
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0">idle submit</d>"#,
        ));
        worker.submit(1, items, LayoutParams::default(), Arc::new(font));
        let mut got = false;
        for _ in 0..200 {
            if worker.try_recv_result().is_some() {
                got = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(got, "空闲后 submit 必须能被处理（不死锁）");
    }

    /// 防回归：连续 submit 多个任务，worker 只执行最新（聚合丢弃），
    /// 且旧任务不再产生结果（generation 只见最新）。
    #[test]
    fn worker_aggregates_stale_queued_tasks() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let worker = LayoutWorker::spawn();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // 先用一个大任务占住 worker，制造确定性窗口：提交期间 worker 忙于它，
        // 后续 8 个短任务必然全部入队，随后被聚合为一次执行。否则主线程在
        // CPU 抢占下提交变慢，worker 会穿插执行多个任务，聚合断言不成立。
        let mut big = String::from("<i>");
        for i in 0..3000 {
            big.push_str(&format!(
                r#"<d p="{}.0,1,25,16777215,0,0,0,0,0">big{i}</d>"#,
                i as f64 / 60.0
            ));
        }
        big.push_str("</i>");
        worker.submit(
            0,
            Arc::new(parse_danmaku_xml(&big)),
            LayoutParams::default(),
            Arc::new(font.clone()),
        );
        let items = Arc::new(parse_danmaku_xml(
            r#"<d p="0.0,1,25,16777215,0,0,0,0,0">aggregate</d>"#,
        ));
        for generation in 1..=8 {
            worker.submit(
                generation,
                Arc::clone(&items),
                LayoutParams::default(),
                Arc::new(font.clone()),
            );
        }
        // 收集到 generation=8 的结果为止：worker 可能在 8 次 submit 之间
        // 抢先完成早期任务（此时首个结果不是 8），首个非空即退会漏掉 8。
        let mut generations = Vec::new();
        for _ in 0..200 {
            while let Some(r) = worker.try_recv_result() {
                generations.push(r.generation);
            }
            if generations.last() == Some(&8) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!generations.is_empty());
        assert!(
            generations.len() <= 2,
            "8 个排队任务应聚合为至多 2 次执行，实际 {generations:?}"
        );
        assert_eq!(*generations.iter().max().unwrap(), 8);
    }

    #[test]
    fn measure_multiline_height_scales_with_line_count() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let (w1, h1) = font.measure("test", 25.0);
        let (w2, h2) = font.measure("test\ntest", 25.0);
        assert!((h2 - h1 * 2.0).abs() < 0.5, "两行高度应约为单行 2 倍: {h2} vs {h1}");
        assert!((w2 - w1).abs() < 0.5, "宽度应取各行 max（同字同宽）");
        let (w3, h3) = font.measure("a\nbb\nccc", 25.0);
        assert!((h3 - h1 * 3.0).abs() < 0.5, "三行高度应为单行 3 倍");
        let (w_a, _) = font.measure("a", 25.0);
        let (w_ccc, _) = font.measure("ccc", 25.0);
        assert!((w3 - w_ccc).abs() < 0.5, "三行宽度应取最宽行 ccc");
        assert!(w3 > w_a, "最宽行 ccc 应宽于 a");
    }
}
