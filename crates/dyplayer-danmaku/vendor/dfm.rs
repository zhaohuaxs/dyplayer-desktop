//! DyPlayer adapter for the NipaPlay DFM+ layout core.
//!
//! This module keeps the DFM+ prepare/frame-query contract intact while removing
//! the Flutter Rust Bridge handle store. The input is prepared danmaku content,
//! viewport, and user config; the output is a prepared layout plus per-media-time
//! frame positions that the host maps into its native GPU render plan.
//!
//! Vendored into DyPlayer (crates/dyplayer-danmaku) from liberika v0.2.1
//! (https://github.com/AimesSoft/liberika, MPL-2.0), where it originated as the
//! Erika adapter. Behavioral semantics are kept intact; only crate module paths
//! were adapted. Later revision: track allocation now runs on the gridded
//! row engine in `dfm_core::retainer` (deviates from upstream liberika);
//! items are sorted by departure time before placement, and `track_index` /
//! `track_count` are reported in grating-row units. `display_area`
//! constrains scroll danmaku only — fixed top/bottom danmaku always lay out
//! over the full view height (also deviates from upstream).

use rustc_hash::{FxHashMap, FxHasher};

use std::collections::HashMap;

use crate::dfm_core::{
    filters::{FilterContext, FilterSystem},
    model::{DanmakuItem, DanmakuType, Duration, GlobalFlags},
    retainer::{DanmakuRetainer, GRATING},
};
use crate::normalize::{DyStyle, SpecialData};
use crate::outline::{raster_radius, resolve_width_px};
use std::hash::{Hash, Hasher};

const STATIC_DURATION_MS: i64 = 3800;

#[derive(Debug, Clone)]
pub struct PrepareItem {
    pub source_id: u64,
    pub time_seconds: f64,
    pub text: String,
    pub type_code: i32,
    pub color_argb: i32,
    pub opacity: f32,
    pub is_me: bool,
    pub font_size: f32,
    pub paint_width: f64,
    pub paint_height: f64,
    /// dy 扩展渐变样式（透传到渲染层；不参与布局碰撞）。
    pub style: Option<DyStyle>,
    /// mode7 高级弹幕数据（已按视口还原；透传到渲染层）。
    pub special: Option<SpecialData>,
}

#[derive(Debug, Clone)]
pub struct PrepareRequest<'a> {
    pub items: Vec<PrepareItem>,
    pub width: f64,
    pub height: f64,
    pub font_size: f64,
    pub display_area: f64,
    pub scroll_duration_seconds: f64,
    pub allow_stacking: bool,
    pub allow_scroll_overwrite: bool,
    pub merge_danmaku: bool,
    pub max_quantity: Option<u32>,
    pub max_lines_per_type: Option<u32>,
    pub track_gap_ratio: f64,
    pub outline_width: f64,
    pub preferred_tracks: &'a HashMap<u64, usize>,
    pub block_words: Vec<String>,
    pub block_top: bool,
    pub block_bottom: bool,
    pub block_scroll: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedLayout {
    pub width: f64,
    pub height: f64,
    pub scroll_duration_seconds: f64,
    pub static_duration_seconds: f64,
    pub items: Vec<PreparedItem>,
    pub item_times: Vec<f64>,
    pub track_count: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedItem {
    pub source_id: u64,
    pub time_seconds: f64,
    pub text: String,
    pub type_code: i32,
    pub color_argb: i32,
    pub opacity: f32,
    pub is_me: bool,
    pub font_size: f64,
    pub font_size_multiplier: f64,
    pub count_text: Option<String>,
    pub duplicate_count: u32,
    pub track_index: i32,
    pub y_position: f64,
    pub width: f64,
    pub height: f64,
    pub scroll_speed: f64,
    pub is_filtered: bool,
    pub duration_seconds: f64,
    pub is_scroll: bool,
    pub centered_x: f64,
    /// dy 扩展渐变样式（渲染层消费；target=Outline 描边渐变文本白，
    /// target=Fill 文本渐变描边黑）。
    pub style: Option<DyStyle>,
    /// mode7 高级弹幕数据（渲染层与帧查询消费）。
    pub special: Option<SpecialData>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameItem {
    pub item_index: usize,
    pub track_index: i32,
    pub x: f64,
    pub y: f64,
    pub offstage_x: f64,
    /// 帧时插值 alpha（mode7 渐变；普通弹幕恒 1.0）。
    pub alpha: f32,
}

pub fn prepare_layout(request: PrepareRequest) -> Result<PreparedLayout, String> {
    let width = request.width.max(1.0) as f32;
    let height = request.height.max(1.0) as f32;

    let display_area = request.display_area.clamp(0.1, 1.0) as f32;
    let scroll_dur_secs = request.scroll_duration_seconds.max(1.0);
    let scroll_dur_ms = (scroll_dur_secs * 1000.0) as i64;
    let global_flags = GlobalFlags::default();
    let outline_width = request.outline_width.max(0.0) as f32;


    let mut items = request
        .items
        .into_iter()
        .map(|raw| {
            let danmaku_type = DanmakuType::from_code(raw.type_code);
            let dur_ms = if danmaku_type.is_scroll() {
                scroll_dur_ms
            } else if danmaku_type == DanmakuType::Special {
                let d = raw
                    .special
                    .as_ref()
                    .map(|s| s.duration_s)
                    .unwrap_or(3.8);
                (d.max(0.1) * 1000.0) as i64
            } else {
                STATIC_DURATION_MS
            };
            let mut item = DanmakuItem::new(
                (raw.time_seconds * 1000.0) as i64,
                raw.text,
                raw.color_argb as u32,
                raw.font_size.max(1.0),
                danmaku_type,
                dur_ms,
            );
            item.alpha = (raw.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
            if raw.paint_width > 0.0 && raw.paint_height > 0.0 {
                // R15：用每条实际字号算描边外扩（raster_radius 取整后 ≥ 渲染
                // 半径），保证碰撞边界包住真实像素外包围框。旧代码用全局
                // 基准字号算 outline_px，混合源字号下碰撞边界可能不足。
                let per_r = raster_radius(resolve_width_px(raw.font_size, outline_width)) as f32;
                item.paint_width = raw.paint_width as f32 + per_r * 2.0;
                item.paint_height = raw.paint_height as f32;
                if item.danmaku_type.is_scroll() {
                    let distance = width + item.paint_width;
                    item.step_x = distance / item.duration_ms as f32;
                }
                item.flags.measure = global_flags.measure_flag;
            }
            (raw.source_id, raw.is_me, raw.font_size.max(1.0), 1u32, item, raw.style, raw.special)
        })
        .collect::<Vec<_>>();

    // Sort by departure time (stable): the gridded retainer's catch-up
    // prediction assumes existing entries depart no later than the entry
    // being placed. Indices are assigned after sorting so displaced-index
    // lookups stay in sync with this ordering.
    items.sort_by_key(|a| a.4.time_ms);
    for (i, (_, _, _, _, item, _, _)) in items.iter_mut().enumerate() {
        item.index = i as u32;
    }

    for (_, _, _, _, item, _, _) in &mut items {
        // R15：用每条实际字号算描边（与 149 行碰撞外扩同源）。
        let per_outline_px = resolve_width_px(item.text_size, outline_width);
        item.measure_with_outline(width, height, &global_flags, per_outline_px);
    }

    if request.merge_danmaku {
        merge_duplicate_items(&mut items);
    }

    let mut filter_sys = FilterSystem::default();
    if let Some(q) = request.max_quantity {
        filter_sys.max_quantity = Some(q);
    }
    if let Some(max) = request.max_lines_per_type {
        for ty in [
            DanmakuType::ScrollRL,
            DanmakuType::ScrollLR,
            DanmakuType::FixTop,
            DanmakuType::FixBottom,
        ] {
            filter_sys.max_lines.insert(ty, max);
        }
    }
    if request.block_scroll {
        filter_sys.blocked_types.insert(DanmakuType::ScrollRL);
        filter_sys.blocked_types.insert(DanmakuType::ScrollLR);
    }
    if request.block_top {
        filter_sys.blocked_types.insert(DanmakuType::FixTop);
    }
    if request.block_bottom {
        filter_sys.blocked_types.insert(DanmakuType::FixBottom);
    }
    filter_sys.duplicate_merge = request.merge_danmaku;
    filter_sys.set_block_words(&request.block_words);

    let scroll_duration = Duration::new(scroll_dur_ms);
    let mut ctx = FilterContext {
        timer_ms: 0,
        index_in_screen: 0,
        screen_size: items.len(),
        frame_elapsed_ms: 0,
        global_flags,
        scroll_duration,
    };
    for (i, (_, _, _, _, item, _, _)) in items.iter_mut().enumerate() {
        if item.is_filtered {
            continue;
        }
        ctx.index_in_screen = i;
        filter_sys.filter_primary(item, &ctx);
    }

    let track_gap_ratio = request.track_gap_ratio.clamp(0.0, 2.0) as f32;
    let mut retainer = DanmakuRetainer::new(2.0, track_gap_ratio);
    let type_order = [
        DanmakuType::ScrollRL,
        DanmakuType::ScrollLR,
        DanmakuType::FixTop,
        DanmakuType::FixBottom,
    ];
    let mut type_indices: [Vec<usize>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (i, (_, _, _, _, item, _, _)) in items.iter().enumerate() {
        if item.is_filtered {
            continue;
        }
        match item.danmaku_type {
            DanmakuType::ScrollRL => type_indices[0].push(i),
            DanmakuType::ScrollLR => type_indices[1].push(i),
            DanmakuType::FixTop => type_indices[2].push(i),
            DanmakuType::FixBottom => type_indices[3].push(i),
            DanmakuType::Special => {}
        }
    }

    for (type_idx, _) in type_order.iter().enumerate() {
        for &i in &type_indices[type_idx] {
            let is_me = items[i].1;
            let preferred_track = request.preferred_tracks.get(&items[i].0).copied();
            let (placed, displaced_index) = retainer.fix_with_options_and_preferred_track(
                &mut items[i].4,
                width,
                height,
                &global_flags,
                display_area,
                is_me,
                request.allow_stacking,
                request.allow_scroll_overwrite,
                preferred_track,
            );
            if !placed {
                items[i].4.is_filtered = true;
                items[i].4.filter_param = 99;
                continue;
            }
            if exceeds_max_line(
                &items[i].4,
                request.max_lines_per_type,
                track_gap_ratio,
                height,

            ) {
                items[i].4.is_filtered = true;
                items[i].4.filter_param = 99;
                continue;
            }
            for &displaced in &displaced_index {
                if displaced < items.len() && !items[displaced].4.is_filtered {
                    items[displaced].4.is_filtered = true;
                    items[displaced].4.filter_param = 99;
                }
            }
        }
    }

    let mut prepared_items = Vec::with_capacity(items.len());
    for (source_id, is_me, raw_font_size, duplicate_count, item, style, special) in &mut items {
        if item.is_filtered {
            continue;
        }
        let type_code = item.danmaku_type as i32;
        let is_scroll = item.danmaku_type.is_scroll();
        let centered_x = if is_scroll {
            0.0
        } else {
            (width as f64 - item.paint_width as f64) / 2.0
        };
        prepared_items.push(PreparedItem {
            source_id: *source_id,
            time_seconds: item.time_ms as f64 / 1000.0,
            text: std::mem::take(&mut item.text),
            type_code,
            color_argb: item.text_color as i32,
            opacity: item.alpha as f32 / 255.0,
            is_me: *is_me,
            font_size: f64::from(*raw_font_size),
            font_size_multiplier: 1.0,
            count_text: None,
            duplicate_count: *duplicate_count,
            track_index: track_index_from_y(item, height),
            y_position: item.y as f64,
            width: item.paint_width as f64,
            height: item.paint_height as f64,
            scroll_speed: item.step_x as f64 * 1000.0,
            is_filtered: item.is_filtered,
            duration_seconds: item.duration_ms as f64 / 1000.0,
            is_scroll,
            centered_x,
            style: style.take(),
            special: special.take(),
        });
    }

    prepared_items.sort_by(|a, b| {
        a.time_seconds
            .partial_cmp(&b.time_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let item_times = prepared_items.iter().map(|i| i.time_seconds).collect();
    Ok(PreparedLayout {
        width: width as f64,
        height: height as f64,
        scroll_duration_seconds: scroll_dur_secs,
        static_duration_seconds: STATIC_DURATION_MS as f64 / 1000.0,
        items: prepared_items,
        item_times,
        // track_count reports the scroll row pool (display_area-constrained);
        // fixed danmaku lay out over the full height and have no single
        // reported count.
        track_count: ((height * display_area / GRATING).floor().max(1.0)) as i32,
    })
}

/// Grating-row index of a placed item. Scroll/top items report their top row
/// (row 0 = screen top); bottom items report rows counted up from the bottom
/// (row 0 = lowest band, against the full-height bottom edge — display_area
/// applies to scroll danmaku only). The 2.0 offset mirrors the retainer's
/// margin.
fn track_index_from_y(item: &DanmakuItem, height: f32) -> i32 {
    if !item.y.is_finite() || item.y < 0.0 {
        return 0;
    }
    match item.danmaku_type {
        DanmakuType::FixBottom => {
            let y_bottom = item.y + item.paint_height;
            ((height - 2.0 - y_bottom).max(0.0) / GRATING).round() as i32
        }
        _ => ((item.y - 2.0).max(0.0) / GRATING).round() as i32,
    }
}

pub fn frame_candidate_count(layout: &PreparedLayout, current_time: f64) -> usize {
    let max_dur = layout
        .scroll_duration_seconds
        .max(layout.static_duration_seconds);
    let end_idx = upper_bound(&layout.item_times, current_time);
    let window_start = current_time - max_dur;
    let start_idx = lower_bound(&layout.item_times[..end_idx], window_start);
    end_idx.saturating_sub(start_idx)
}

/// 遍历 `current_time` 时刻的所有可见弹幕帧项。
///
/// 查询语义：spawn_time ∈ (current_time − max_dur, current_time]，
/// 再按各条 duration 过滤。`item_times` 按 spawn_time 升序，故用两次
/// `partition_point` 双侧夹逼出候选窗口，复杂度 O(log N + W)，W 为窗口内条数。
/// start 的二分限定在 `[..end_idx]` 上，既表达"start ≤ end"不变量又缩小搜索范围。
pub fn for_each_frame_item(
    layout: &PreparedLayout,
    current_time: f64,
    mut visit: impl FnMut(FrameItem),
) {
    let width = layout.width;
    let scroll_dur = layout.scroll_duration_seconds;
    let static_dur = layout.static_duration_seconds;
    let max_dur = scroll_dur.max(static_dur);
    let end_idx = upper_bound(&layout.item_times, current_time);
    let window_start = current_time - max_dur;
    let start_idx = lower_bound(&layout.item_times[..end_idx], window_start);

    for i in start_idx..end_idx {
        let item = &layout.items[i];
        let elapsed = current_time - item.time_seconds;
        if elapsed < 0.0 {
            continue;
        }
        if !item.is_scroll && elapsed >= item.duration_seconds {
            // R16：统一 [start,end) 生命周期——与 retainer 的
            // `current_time_ms < end_ms()` 一致，end 时刻不显示，
            // 避免暂停/精确 seek 在端点同时输出 A/B 重叠。
            continue;
        }
        let (x, y, alpha, offstage_x) = if item.is_scroll {
            let speed = item.scroll_speed;
            let (x, offstage_x) = if item.type_code == DanmakuType::ScrollLR as i32 {
                (speed * elapsed - item.width, -item.width)
            } else {
                (width - speed * elapsed, width + item.width)
            };
            (x, item.y_position, 1.0_f32, offstage_x)
        } else if let Some(sp) = &item.special {
            let progress = (elapsed / sp.duration_s).clamp(0.0, 1.0);
            let x = sp.x1 + (sp.x2 - sp.x1) * progress;
            let y = sp.y1 + (sp.y2 - sp.y1) * progress;
            let a = sp.alpha_from + (sp.alpha_to - sp.alpha_from) * progress as f32;
            (x, y, a, width)
        } else {
            (item.centered_x, item.y_position, 1.0_f32, width)
        };
        if item.is_scroll && x < -item.width {
            continue;
        }
        if y < 0.0 {
            continue;
        }
        visit(FrameItem {
            item_index: i,
            track_index: item.track_index,
            x,
            y,
            offstage_x,
            alpha,
        });
    }
}

/// prepare 中间态条目：(source_id, is_me, 源字号, 合并计数, vendor 条目, dy 样式)。
type PrepareEntry = (u64, bool, f32, u32, DanmakuItem, Option<DyStyle>, Option<SpecialData>);

fn merge_duplicate_items(items: &mut [PrepareEntry]) {
    // 合并键 = 文本 + 样式：同文本不同渐变的 dy 扩展弹幕不合并（各自
    // 保留渐变观感）。普通弹幕 style 均为 None，行为与旧版一致。
    let mut merge_map: FxHashMap<u64, (usize, u32)> = FxHashMap::default();
    let mut dup_indices = Vec::new();
    for (i, (_, _, _, _, item, style, _)) in items.iter().enumerate() {
        let text_hash = fxhash_str(&item.text);
        match merge_map.get_mut(&text_hash) {
            Some((first_idx, count)) => {
                if item.text == items[*first_idx].4.text && *style == items[*first_idx].5 {
                    dup_indices.push(i);
                    *count += 1;
                }
            }
            None => {
                merge_map.insert(text_hash, (i, 1));
            }
        }
    }
    for idx in dup_indices {
        items[idx].4.is_filtered = true;
        items[idx].4.filter_param = 99;
    }
    for &(first_idx, count) in merge_map.values() {
        if count > 1 {
            items[first_idx].3 = count;
            items[first_idx].4.text.push_str(&format!(" x{count}"));
        }
    }
}

fn fxhash_str(s: &str) -> u64 {
    let mut hasher = FxHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

fn exceeds_max_line(
    item: &DanmakuItem,
    max_lines_per_type: Option<u32>,
    track_gap_ratio: f32,
    height: f32,
) -> bool {
    let Some(max_lines) = max_lines_per_type else {
        return false;
    };
    if max_lines == 0 {
        return true;
    }
    let track_height = (item.paint_height + item.paint_height * track_gap_ratio).max(1.0);
    // Fixed danmaku count rows over the full view height (display_area
    // applies to scroll danmaku only).
    let row = match item.danmaku_type {
        DanmakuType::FixBottom => ((height - item.y) / track_height).ceil().max(1.0) as u32 - 1,
        _ => ((item.y - 2.0) / track_height).floor().max(0.0) as u32,
    };
    row >= max_lines
}

fn lower_bound(times: &[f64], target: f64) -> usize {
    times.partition_point(|&time| time < target)
}

fn upper_bound(times: &[f64], target: f64) -> usize {
    times.partition_point(|&time| time <= target)
}
