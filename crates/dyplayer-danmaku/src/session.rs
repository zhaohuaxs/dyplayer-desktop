//! 弹幕会话（自有层，M2）：单条弹幕轨 + offset + generation 管理。
//!
//! TDD 6.5 契约：
//! - 换轨/参数重建时 generation+1，渲染侧（render 的 atlas/Mesh 缓存）据此
//!   丢弃旧 generation 的 prepared layout 与缓存
//! - 延迟（offset）零成本热更新：帧查询时应用 `t' = t - offset`（时间平移
//!   不改变相对顺序与碰撞结果），无需重 prepare
//! - 媒体时间由调用方从 dyplayer-mpv 的 MediaClock 查询后传入（单源时钟）

use std::sync::Arc;

use crate::dfm::{FrameItem, PreparedItem, PreparedLayout, for_each_frame_item};

use crate::layout::{DanmakuFont, LayoutParams, LayoutWorker};
use crate::normalize::{NormalizedItem, parse_danmaku_xml};

/// 一路弹幕的完整会话状态：源数据、布局参数、当前生效 layout、后台 worker。
///
/// 所有方法在 UI 线程调用；`poll` 每帧调用一次收取 worker 结果。
pub struct DanmakuSession {
    font: Arc<DanmakuFont>,
    params: LayoutParams,
    items: Arc<Vec<NormalizedItem>>,
    generation: u64,
    layout: Option<Arc<PreparedLayout>>,
    layout_generation: u64,
    /// 生成当前 layout 时使用的字体（R13：渲染须用 layout 生成时的 font，
    /// 而非最新 font——set_font 后旧 layout 沿用至新结果就绪，此期间渲染
    /// 用旧 font 才与新字形度量一致）。
    layout_font: Option<Arc<DanmakuFont>>,
    offset_seconds: f64,
    worker: LayoutWorker,
}

impl DanmakuSession {
    pub fn new(font: DanmakuFont, params: LayoutParams) -> Self {
        Self {
            font: Arc::new(font),
            params,
            items: Arc::new(Vec::new()),
            generation: 0,
            layout: None,
            layout_generation: 0,
            layout_font: None,
            offset_seconds: 0.0,
            worker: LayoutWorker::spawn(),
        }
    }

    /// 换轨：解析 XML 并提交后台 prepare；generation+1，旧 layout 沿用至
    /// 新结果就绪（无闪烁）。空解析结果直接清空显示。
    pub fn load_xml(&mut self, xml: &str) {
        let items = Arc::new(parse_danmaku_xml(xml));
        self.load_items(items);
    }

    /// R12-B1：从已解析的 items 加载（后台线程完成 read+parse 后调用）。
    pub fn load_items(&mut self, items: Arc<Vec<NormalizedItem>>) {
        self.generation += 1;
        if items.is_empty() {
            self.items = items;
            self.layout = None;
            return;
        }
        self.items = Arc::clone(&items);
        self.worker.submit(
            self.generation,
            items,
            self.params.clone(),
            Arc::clone(&self.font),
        );
    }

    /// 布局参数更新：与现值 diff，变化才 generation+1 并提交重 prepare。
    pub fn set_params(&mut self, params: LayoutParams) {
        if params == self.params {
            return;
        }
        self.params = params;
        self.generation += 1;
        if self.items.is_empty() {
            return;
        }
        self.worker.submit(
            self.generation,
            Arc::clone(&self.items),
            self.params.clone(),
            Arc::clone(&self.font),
        );
    }

    /// 字体热替换（设置面板切换字体）：font 参与 prepare 测量，须重排。
    /// generation+1；有轨道时重提交 worker，渲染侧沿用旧 layout 至新结果
    /// 就绪（与 set_params 同族无闪烁契约）。
    pub fn set_font(&mut self, font: DanmakuFont) {
        self.font = Arc::new(font);
        self.generation += 1;
        if self.items.is_empty() {
            return;
        }
        self.worker.submit(
            self.generation,
            Arc::clone(&self.items),
            self.params.clone(),
            Arc::clone(&self.font),
        );
    }

    /// 延迟（秒，正=弹幕延后）：即时生效，零成本（查询侧时间平移）。
    pub fn set_offset_seconds(&mut self, offset_seconds: f64) {
        self.offset_seconds = if offset_seconds.is_finite() {
            offset_seconds
        } else {
            0.0
        };
    }


    /// 收取 worker 结果（帧循环调用）：generation 匹配才替换 layout，
    /// 失败/过时的结果一律丢弃并沿用旧 layout（无黑帧）。
    pub fn poll(&mut self) {
        while let Some(result) = self.worker.try_recv_result() {
            if result.generation != self.generation {
                continue;
            }
            if let Ok(layout) = result.layout {
                self.layout = Some(Arc::new(layout));
                self.layout_generation = result.generation;
                // R13：记录生成此 layout 的 font，渲染侧用它与度量一致。
                self.layout_font = Some(Arc::clone(&self.font));
            }
        }
    }

    /// 当前生效的 prepared layout（render 层按 layout_generation 对齐缓存）。
    pub fn layout(&self) -> Option<&Arc<PreparedLayout>> {
        self.layout.as_ref()
    }

    /// 当前生效 layout 的 generation（测试断言重 prepare 生效用）。
    #[cfg(test)]
    pub(crate) fn layout_generation(&self) -> u64 {
        self.layout_generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn params(&self) -> &LayoutParams {
        &self.params
    }

    pub fn font(&self) -> &Arc<DanmakuFont> {
        &self.font
    }

    /// 生成当前生效 layout 的字体（R13：渲染用此与 layout 度量一致；
    /// 无 layout 时回退最新 font）。
    pub fn layout_font(&self) -> &Arc<DanmakuFont> {
        self.layout_font.as_ref().unwrap_or(&self.font)
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// 遍历当前媒体时刻的可见弹幕（已应用 offset）。
    ///
    /// `media_time_seconds` 来自 dyplayer-mpv 的 MediaClock；`visit` 收到
    /// 帧位置（DFM+ 计算的 x/y）与对应样式条目（经 item_index 回查）。
    /// 条目引用绑定 `&self`（渲染层可收集后统一生成 Mesh）。
    pub fn for_each_visible_item<'a>(
        &'a self,
        media_time_seconds: f64,
        mut visit: impl FnMut(FrameItem, &'a PreparedItem),
    ) {
        let Some(layout) = &self.layout else {
            return;
        };
        let query_time = media_time_seconds - self.offset_seconds;
        if !query_time.is_finite() {
            return;
        }
        for_each_frame_item(layout, query_time, |frame| {
            let item = &layout.items[frame.item_index];
            visit(frame, item);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dfm_core::model::DanmakuType;
    use crate::layout::DanmakuFont;

    fn session() -> Option<DanmakuSession> {
        let font = DanmakuFont::from_system()?;
        Some(DanmakuSession::new(font, LayoutParams::default()))
    }

    fn wait_for_layout(sess: &mut DanmakuSession) {
        for _ in 0..300 {
            sess.poll();
            if sess.layout().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("layout never arrived");
    }

    fn wait_for_generation(sess: &mut DanmakuSession, generation: u64) {
        for _ in 0..300 {
            sess.poll();
            if sess.layout_generation() >= generation {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("layout for generation {generation} never arrived");
    }

    /// 复现 UI 参数调整序列：每步统计 t=5s 可见数与 y 范围。
    #[test]
    fn param_adjustments_keep_danmaku_visible() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut xml = String::from("<i>");
        for i in 0..120 {
            let mode = if i % 6 == 4 { 5 } else if i % 6 == 5 { 4 } else { 1 };
            xml.push_str(&format!(
                r#"<d p="{:.3},{mode},25,16777215,0,0,0,0,0">弹幕{i}号</d>"#,
                i as f64 / 12.0
            ));
        }
        xml.push_str("</i>");
        sess.load_xml(&xml);
        wait_for_layout(&mut sess);

        // (总数, 滚动 y_max, 固定 y_min, 固定 y_max)
        let stats = |sess: &DanmakuSession, t: f64| -> (usize, f64, f64, f64) {
            let mut count = 0usize;
            let mut scroll_y_max = f64::MIN;
            let mut fixed_y_min = f64::MAX;
            let mut fixed_y_max = f64::MIN;
            sess.for_each_visible_item(t, |frame, item| {
                count += 1;
                if item.is_scroll {
                    scroll_y_max = scroll_y_max.max(frame.y + item.height);
                } else {
                    fixed_y_min = fixed_y_min.min(frame.y);
                    fixed_y_max = fixed_y_max.max(frame.y + item.height);
                }
            });
            (count, scroll_y_max, fixed_y_min, fixed_y_max)
        };

        let check = |sess: &mut DanmakuSession, generation: u64, label: &str, area: f64, vh: f64| {
            wait_for_generation(sess, generation);
            let (count, scroll_y_max, fixed_y_min, fixed_y_max) = stats(sess, 5.0);
            eprintln!(
                "{label}: visible={count} scroll_y_max={scroll_y_max:.1} fixed_y=[{fixed_y_min:.1},{fixed_y_max:.1}] scroll_limit={:.0}",
                vh * area
            );
            assert!(count > 0, "{label}: 无弹幕可见");
            assert!(
                scroll_y_max <= vh * area + 5.0,
                "{label}: 滚动弹幕超出显示区域"
            );
            // 固定弹幕不受显示区域约束：顶部贴屏幕顶、底部贴屏幕底。
            assert!(fixed_y_min >= 0.0, "{label}: 固定弹幕越出屏幕顶部");
            assert!(fixed_y_max <= vh + 5.0, "{label}: 固定弹幕越出屏幕底部");
        };

        let base = LayoutParams::default();
        check(&mut sess, 1, "默认(字号30/区域1.0/速度1.0)", base.display_area, base.viewport_height);

        let mut p = base.clone();
        p.base_font_size = 48.0;
        sess.set_params(p.clone());
        check(&mut sess, 2, "字号48", p.display_area, p.viewport_height);

        let mut p = base.clone();
        p.base_font_size = 12.0;
        sess.set_params(p.clone());
        check(&mut sess, 3, "字号12", p.display_area, p.viewport_height);

        let mut p = base.clone();
        p.display_area = 0.25;
        sess.set_params(p.clone());
        check(&mut sess, 4, "区域0.25", p.display_area, p.viewport_height);

        let mut p = base.clone();
        p.scroll_speed_factor = 2.0;
        sess.set_params(p.clone());
        check(&mut sess, 5, "速度2.0", p.display_area, p.viewport_height);

        let mut p = base.clone();
        p.scroll_speed_factor = 0.5;
        sess.set_params(p.clone());
        check(&mut sess, 6, "速度0.5", p.display_area, p.viewport_height);

        sess.set_params(base.clone());
        check(&mut sess, 7, "恢复默认", base.display_area, base.viewport_height);
    }

    /// 显示区域仅约束滚动弹幕：area 收缩后滚动弹幕被压进顶部区域，
    /// 顶部弹幕仍从屏幕顶部起排、底部弹幕仍贴屏幕底边。
    #[test]
    fn display_area_constrains_scroll_only() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut xml = String::from("<i>");
        for i in 0..12 {
            let mode = match i % 3 {
                0 => 1, // ScrollRL
                1 => 5, // FixTop
                _ => 4, // FixBottom
            };
            xml.push_str(&format!(
                r#"<d p="4.000,{mode},25,16777215,0,0,0,0,0">弹幕{i}号</d>"#
            ));
        }
        xml.push_str("</i>");
        sess.load_xml(&xml);
        wait_for_layout(&mut sess);

        let p = LayoutParams {
            display_area: 0.25,
            ..Default::default()
        };
        let vh = p.viewport_height;
        sess.set_params(p);
        wait_for_generation(&mut sess, 2);
        let mut scroll_y_max = f64::MIN;
        let mut top_y_min = f64::MAX;
        let mut bottom_y_max = f64::MIN;
        sess.for_each_visible_item(5.0, |frame, item| {
            if item.is_scroll {
                scroll_y_max = scroll_y_max.max(frame.y + item.height);
            } else if item.type_code == DanmakuType::FixBottom as i32 {
                bottom_y_max = bottom_y_max.max(frame.y + item.height);
            } else {
                top_y_min = top_y_min.min(frame.y);
            }
        });
        assert!(
            scroll_y_max <= vh * 0.25 + 5.0,
            "滚动弹幕应被压进顶部 25% 区域，got {scroll_y_max}"
        );
        assert!(
            top_y_min < vh * 0.25,
            "顶部弹幕应仍从屏幕顶部起排，got {top_y_min}"
        );
        assert!(
            bottom_y_max > vh * 0.75,
            "底部弹幕应仍贴屏幕底边，got {bottom_y_max}"
        );
    }

    #[test]
    fn set_font_bumps_generation_and_reprepares() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        sess.load_xml(r#"<d p="0.0,1,25,16777215,0,0,0,0,0">hello</d>"#);
        wait_for_layout(&mut sess);
        assert_eq!(sess.generation(), 1);
        assert_eq!(sess.layout_generation(), 1);

        let Some(new_font) = DanmakuFont::from_system() else {
            eprintln!("skip: re-load system font failed");
            return;
        };
        sess.set_font(new_font);
        assert_eq!(sess.generation(), 2);
        wait_for_generation(&mut sess, 2);
        assert_eq!(sess.layout_generation(), 2);
        assert_eq!(sess.layout().unwrap().items.len(), 1);
    }

    #[test]
    fn set_font_without_items_bumps_generation_without_worker_roundtrip() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        let Some(new_font) = DanmakuFont::from_system() else {
            eprintln!("skip: re-load system font failed");
            return;
        };
        sess.set_font(new_font);
        assert_eq!(sess.generation(), 1);
        assert!(sess.layout().is_none());
    }

    #[test]
    fn load_xml_bumps_generation_and_prepares() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        assert_eq!(sess.generation(), 0);
        assert!(sess.layout().is_none());
        sess.load_xml(r#"<d p="0.0,1,25,16777215,0,0,0,0,0">hello</d>"#);
        assert_eq!(sess.generation(), 1);
        assert_eq!(sess.item_count(), 1);
        wait_for_layout(&mut sess);
        assert_eq!(sess.layout_generation(), 1);
        assert_eq!(sess.layout().unwrap().items.len(), 1);
    }

    #[test]
    fn empty_xml_clears_layout_without_worker_roundtrip() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        sess.load_xml(r#"<d p="0.0,1,25,16777215,0,0,0,0,0">hello</d>"#);
        wait_for_layout(&mut sess);
        assert!(sess.layout().is_some());
        sess.load_xml("");
        assert_eq!(sess.generation(), 2);
        assert!(sess.layout().is_none());
        assert_eq!(sess.item_count(), 0);
    }

    #[test]
    fn identical_params_do_not_bump_generation() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        sess.set_params(LayoutParams::default());
        assert_eq!(sess.generation(), 0);
        let changed = LayoutParams {
            display_area: 0.5,
            ..Default::default()
        };
        sess.set_params(changed.clone());
        assert_eq!(sess.generation(), 1);
        sess.set_params(changed);
        assert_eq!(sess.generation(), 1);
    }

    #[test]
    fn offset_applies_at_query_side_only() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        sess.load_xml(r#"<d p="10.0,1,25,16777215,0,0,0,0,0">delayed</d>"#);
        wait_for_layout(&mut sess);
        let count_at = |sess: &DanmakuSession, t: f64| {
            let mut count = 0;
            sess.for_each_visible_item(t, |_, _| count += 1);
            count
        };
        // 弹幕在 t=10 出现（滚动窗口内可见）
        assert_eq!(count_at(&sess, 10.5), 1);
        assert_eq!(count_at(&sess, 8.0), 0);
        // offset +2：查询时刻平移，t=12 才看到 t=10 的弹幕
        sess.set_offset_seconds(2.0);
        assert_eq!(count_at(&sess, 12.5), 1);
        assert_eq!(count_at(&sess, 10.5), 0);
        assert_eq!(sess.generation(), 1, "offset 不得触发重 prepare");
    }

    #[test]
    fn visible_items_report_positions_and_styles() {
        let Some(mut sess) = session() else {
            eprintln!("skip: no system font available");
            return;
        };
        sess.load_xml(r#"<d p="0.0,5,25,16711680,0,0,0,0,0">top</d>"#);
        wait_for_layout(&mut sess);
        let mut seen = 0;
        sess.for_each_visible_item(1.0, |frame, item| {
            assert_eq!(item.text, "top");
            assert_eq!(item.type_code, 5);
            assert_eq!(item.color_argb as u32, 0xFFFF_0000);
            assert_eq!(frame.item_index, 0);
            assert!(frame.y >= 0.0);
            assert!(frame.x >= 0.0);
            seen += 1;
        });
        assert_eq!(seen, 1);
    }

    #[test]
    fn mode7_path_interpolation_and_alpha() {
        let Some(font) = DanmakuFont::from_system() else {
            eprintln!("skip: no system font available");
            return;
        };
        // viewport = 老基准 545×388，date<1384099200 → 坐标 1:1 还原。
        let params = LayoutParams {
            viewport_width: 545.0,
            viewport_height: 388.0,
            ..LayoutParams::default()
        };
        let mut sess = DanmakuSession::new(font, params);
        // 从 (50,245) 移到 (350,0)，duration=4.5s，alpha 0.1→1.0。
        sess.load_xml(r#"<d p="0.0,7,25,16777215,1291190667,0,x,0">["50","245","0.1-1.00","4.50","达成","350","0"]</d>"#);
        wait_for_layout(&mut sess);

        let check = |sess: &DanmakuSession, t: f64, expect_x: f64, expect_y: f64, expect_a: f32| {
            let mut hit = 0;
            sess.for_each_visible_item(t, |frame, item| {
                assert_eq!(item.type_code, 7, "mode7");
                let fx = frame.x;
                let fy = frame.y;
                let fa = frame.alpha;
                assert!((fx - expect_x).abs() < 1.0, "t={t}: x={fx} expect={expect_x}");
                assert!((fy - expect_y).abs() < 1.0, "t={t}: y={fy} expect={expect_y}");
                assert!((fa - expect_a).abs() < 0.01, "t={t}: alpha={fa} expect={expect_a}");
                hit += 1;
            });
            assert_eq!(hit, 1, "t={t}: 应恰好 1 条可见");
        };

        check(&sess, 0.0, 50.0, 245.0, 0.1);
        check(&sess, 2.25, 200.0, 122.5, 0.55);
        // end 时刻不显示（R16 [start,end) 生命周期）。
        let mut hit = 0;
        sess.for_each_visible_item(4.5, |_, _| hit += 1);
        assert_eq!(hit, 0, "duration 结束时刻不显示");
    }
}
