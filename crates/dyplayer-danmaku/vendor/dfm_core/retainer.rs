/// Gridded collision-avoidance layout engine (prepare-time, pre-computed).
///
/// Hybrid design fixing vertical overlap between mixed font sizes:
/// - Screen-fixed grating rows (BiliLocal `Grating`, 10px) instead of
///   per-item track heights: rows are screen geometry, so a tall danmaku on
///   row N and a short danmaku on row N+1 can no longer overlap, which the
///   old per-item `track_height = paint_height * (1 + gap)` could not
///   guarantee (h2 < h1 on adjacent tracks always overlapped).
/// - 2D collision with area-based crowding (after BiliLocal
///   `Mode1::intersects`): vertical pixel overlap times a horizontal width
///   that includes catch-up prediction
///   (`o = f.right - f.speed * s.left / s.speed`), replacing the
///   horizontal-projection-only boolean ported from DFM.
/// - First-fit keeps the sparse-time look identical to the old engine; under
///   overload it degrades to min-crowding soft overlap (least-bad row,
///   nothing dropped or displaced) instead of the old stable-zone overwrite
///   that silently cleared whole tracks.
///
/// Contract with `dfm.rs` is unchanged, with one precondition: items must be
/// fed in time order (earlier `time_ms` first), which `prepare_layout`
/// guarantees by sorting. `fix*` returns whether the item was placed plus
/// indices of displaced danmaku; only `is_me` still displaces (it claims the
/// top row band, clearing whatever occupies it — deliberate product rule).
///
/// Vendored into DyPlayer (crates/dyplayer-danmaku) from liberika v0.2.1
/// (https://github.com/AimesSoft/liberika, MPL-2.0). The track allocation
/// core has since been rewritten (gridded rows + area crowding, after
/// BiliLocal's Process::run / Mode1::intersects) and intentionally deviates
/// from upstream liberika behavior.
use smallvec::SmallVec;
use std::collections::VecDeque;

use crate::dfm_core::model::{DanmakuItem, DanmakuType, GlobalFlags};

/// Screen-fixed grating row height in pixels (BiliLocal `/Danmaku/Grating`
/// default). Decouples row geometry from per-item font sizes.
pub const GRATING: f32 = 10.0;

type DisplacedIndices = SmallVec<[usize; 4]>;

/// Lightweight record stored in grating rows for collision detection.
/// An entry is pushed into every grating row its band covers
/// (`row_start..row_start + row_span`), so a single row lookup sees exactly
/// the entries whose vertical band covers that row.
/// For fixed danmaku, `time_ms` is used as the track's END time (not start
/// time), so that compaction can check if a new danmaku starts after the
/// band is free.
#[derive(Debug, Clone)]
struct TrackEntry {
    time_ms: i64,
    duration_ms: i64,
    paint_width: f32,
    paint_height: f32,
    step_x: f32,
    danmaku_type: DanmakuType,
    danmaku_index: usize,
    /// Top grating row of the placed band.
    row_start: u16,
    /// Number of grating rows the band covers.
    row_span: u16,
}

impl TrackEntry {
    fn from_item(item: &DanmakuItem, index: usize) -> Self {
        Self {
            time_ms: item.time_ms,
            duration_ms: item.duration_ms,
            paint_width: item.paint_width,
            paint_height: item.paint_height,
            step_x: item.step_x,
            danmaku_type: item.danmaku_type,
            danmaku_index: index,
            row_start: 0,
            row_span: 1,
        }
    }

    fn end_ms(&self) -> i64 {
        self.time_ms + self.duration_ms
    }
}

#[derive(Debug, Clone)]
struct TrackData {
    /// Per-grating-row entry lists (fixed types only: their lookups are rare
    /// and their rows hold short time chains).
    tracks: Vec<Vec<TrackEntry>>,
    /// Placed scroll entries in departure order — one copy per entry, no
    /// per-row duplication. Feeding order is time-sorted, so compaction pops
    /// expired entries from the front in O(1) amortized, and burst-mode
    /// crowding scans from the back and stops at the cutoff.
    recent: VecDeque<TrackEntry>,
    last_compact_ms: i64,
}

impl TrackData {
    fn new() -> Self {
        Self {
            tracks: Vec::new(),
            recent: VecDeque::new(),
            last_compact_ms: i64::MIN,
        }
    }

    fn ensure_track_count(&mut self, count: usize) {
        if self.tracks.len() != count {
            self.tracks.resize_with(count, Vec::new);
        }
    }

    /// Drop scroll entries that have fully left the screen. `recent` is in
    /// departure order, so expired entries sit at the front.
    fn compact_recent(&mut self, current_time_ms: i64) {
        if current_time_ms == self.last_compact_ms {
            return;
        }
        self.last_compact_ms = current_time_ms;
        while let Some(front) = self.recent.front() {
            if front.end_ms() <= current_time_ms {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.tracks.clear();
        self.recent.clear();
        self.last_compact_ms = i64::MIN;
    }
}


/// Gridded collision-avoidance engine.
#[derive(Debug, Clone)]
pub struct DanmakuRetainer {
    r2l_tracks: TrackData,
    lr_tracks: TrackData,
    top_tracks: TrackData,
    bottom_tracks: TrackData,
    margin: f32,
    track_gap_ratio: f32,
}

impl DanmakuRetainer {
    pub fn new(margin: f32, track_gap_ratio: f32) -> Self {
        Self {
            r2l_tracks: TrackData::new(),
            lr_tracks: TrackData::new(),
            top_tracks: TrackData::new(),
            bottom_tracks: TrackData::new(),
            margin,
            track_gap_ratio,
        }
    }

    pub fn clear(&mut self) {
        self.r2l_tracks.clear();
        self.lr_tracks.clear();
        self.top_tracks.clear();
        self.bottom_tracks.clear();
    }

    /// Assign a Y position to a danmaku item using gridded collision
    /// avoidance. Returns true if a position was found, false if the item
    /// should be dropped. Returns indices of any displaced danmaku that
    /// should be marked as filtered (only `is_me` displaces).
    pub fn fix(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
    ) -> (bool, DisplacedIndices) {
        self.fix_with_options(
            item,
            view_width,
            view_height,
            flags,
            display_area,
            is_me,
            false,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fix_with_options(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
        allow_stacking: bool,
        allow_scroll_overwrite: bool,
    ) -> (bool, DisplacedIndices) {
        self.fix_with_options_and_preferred_track(
            item,
            view_width,
            view_height,
            flags,
            display_area,
            is_me,
            allow_stacking,
            allow_scroll_overwrite,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fix_with_options_and_preferred_track(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
        allow_stacking: bool,
        allow_scroll_overwrite: bool,
        preferred_track: Option<usize>,
    ) -> (bool, DisplacedIndices) {
        // Each type has independent gridding systems (r2l_tracks, top_tracks,
        // etc.). display_area constrains scroll danmaku only: top/bottom
        // danmaku always lay out over the full view height, so shrinking the
        // display area never lifts bottom danmaku off the screen bottom nor
        // starves top/bottom row pools.
        let effective_height = if item.danmaku_type.is_scroll() {
            view_height * display_area
        } else {
            view_height
        };
        let total_rows = total_rows_for(effective_height);
        let row_span = row_span_for(item.paint_height, self.track_gap_ratio, total_rows);
        let danmaku_index = item.index as usize;

        let entry = TrackEntry::from_item(item, danmaku_index);

        match item.danmaku_type {
            DanmakuType::ScrollRL => {

                match select_scroll_track(
                    &entry,
                    row_span,
                    &mut self.r2l_tracks,
                    total_rows,
                    view_width,
                    is_me,
                    allow_stacking,
                    allow_scroll_overwrite,
                    preferred_track,
                ) {
                    Some((row, displaced)) => {
                        item.y = self.margin + row as f32 * GRATING;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, displaced)
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::ScrollLR => {

                match select_scroll_track(
                    &entry,
                    row_span,
                    &mut self.lr_tracks,
                    total_rows,
                    view_width,
                    is_me,
                    allow_stacking,
                    allow_scroll_overwrite,
                    preferred_track,
                ) {
                    Some((row, displaced)) => {
                        item.y = self.margin + row as f32 * GRATING;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, displaced)
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::FixTop => {
                self.top_tracks.ensure_track_count(total_rows);
                match select_fixed_track(
                    &entry,
                    row_span,
                    &mut self.top_tracks,
                    total_rows,
                    preferred_track,
                    false,
                ) {
                    Some(row) => {
                        item.y = self.margin + row as f32 * GRATING;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, SmallVec::new())
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::FixBottom => {
                self.bottom_tracks.ensure_track_count(total_rows);
                match select_fixed_track(
                    &entry,
                    row_span,
                    &mut self.bottom_tracks,
                    total_rows,
                    preferred_track,
                    true,
                ) {
                    Some(row) => {
                        // Bottom band sits against the bottom edge of the
                        // full view height (display_area does not apply to
                        // fixed danmaku): row 0 of the bottom-up candidate
                        // order is the lowest band.
                        let y_bottom = effective_height
                            - self.margin
                            - (total_rows - row - row_span) as f32 * GRATING;
                        item.y = y_bottom - item.paint_height;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, SmallVec::new())
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::Special => {
                item.y = 0.0;
                item.is_shown = true;
                item.flags.visible = flags.visible_flag;
                (true, SmallVec::new())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Row geometry
// ---------------------------------------------------------------------------

/// Number of screen-fixed grating rows in the effective height.
#[inline]
fn total_rows_for(effective_height: f32) -> usize {
    ((effective_height / GRATING).floor().max(1.0)) as usize
}

/// Number of grating rows a danmaku of `paint_height` occupies, including the
/// configured gap. Quantization itself provides slack (e.g. a 22px danmaku
/// on 10px grating occupies 3 rows = 30px, 8px of visual gap).
#[inline]
fn row_span_for(paint_height: f32, track_gap_ratio: f32, total_rows: usize) -> usize {
    let effective = paint_height * (1.0 + track_gap_ratio.max(0.0));
    let span = ((effective / GRATING).ceil().max(1.0)) as usize;
    span.min(total_rows).max(1)
}

// ---------------------------------------------------------------------------
// Track selection
// ---------------------------------------------------------------------------

/// Select a grating row for a scroll danmaku.
/// Returns (top_row, displaced_indices) or None if the item should be
/// dropped.
///
/// Placement order: preferred row → stacking (unconditional) → first-fit on
/// the crowding map (identical look to the old engine when sparse) → `is_me`
/// claims the top band (the only displacement path) → min-crowding soft
/// overlap when overwriting is allowed → drop.
#[allow(clippy::too_many_arguments)]
fn select_scroll_track(
    new_entry: &TrackEntry,
    new_span: usize,
    track_data: &mut TrackData,
    total_rows: usize,
    view_width: f32,
    is_me: bool,
    allow_stacking: bool,
    allow_scroll_overwrite: bool,
    preferred_track: Option<usize>,
) -> Option<(usize, DisplacedIndices)> {
    track_data.compact_recent(new_entry.time_ms);

    let sta_max = total_rows - new_span;

    if let Some(sta) = preferred_track.filter(|&sta| sta <= sta_max) {
        let can_reuse = allow_stacking
            || !scroll_recent_collide(new_entry, new_span, sta, &track_data.recent, view_width);
        if can_reuse {
            place_recent(track_data, new_entry, sta, new_span);
            return Some((sta, SmallVec::new()));
        }
    }

    if allow_stacking && total_rows > 0 {
        let sta = stack_row_for(new_entry, sta_max);
        place_recent(track_data, new_entry, sta, new_span);
        return Some((sta, SmallVec::new()));
    }

    let thick = build_scroll_crowding(new_entry, new_span, &track_data.recent, sta_max, view_width);

    // First-fit: the first candidate row with zero crowding. This keeps the
    // sparse-time behaviour identical to the old first-non-colliding-track
    // engine.
    if let Some(sta) = thick.iter().position(|&crowding| crowding == 0.0) {
        place_recent(track_data, new_entry, sta, new_span);
        return Some((sta, SmallVec::new()));
    }

    // Self-sent danmaku claims the top band unconditionally, displacing
    // whatever occupies it. Deliberate product rule; the only way a placed
    // entry loses its slot.
    if is_me && total_rows > 0 {
        let mut displaced: DisplacedIndices = SmallVec::new();
        for existing in track_data.recent.iter() {
            if (existing.row_start as usize) < new_span
                && !displaced.contains(&existing.danmaku_index)
            {
                displaced.push(existing.danmaku_index);
            }
        }
        track_data
            .recent
            .retain(|existing| (existing.row_start as usize) >= new_span);
        place_recent(track_data, new_entry, 0, new_span);
        return Some((0, displaced));
    }

    // Overload: soft-overlap at the least crowded row instead of dropping
    // or clearing tracks. Mirrors BiliLocal's min_element placement.
    if allow_scroll_overwrite
        && let Some((best_sta, _)) = thick
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
    {
        place_recent(track_data, new_entry, best_sta, new_span);
        return Some((best_sta, SmallVec::new()));
    }

    None
}

/// Record a scroll placement into the recent queue.
fn place_recent(track_data: &mut TrackData, entry: &TrackEntry, sta: usize, span: usize) {
    let mut placed = entry.clone();
    placed.row_start = sta as u16;
    placed.row_span = span as u16;
    track_data.recent.push_back(placed);
}

/// Select a grating row for a fixed (top/bottom) danmaku.
/// Bottom danmaku fill from the lowest band upward; top danmaku from the
/// highest band downward. Fixed danmaku never soft-overlap: when every band
/// collides, the item is dropped.
fn select_fixed_track(
    new_entry: &TrackEntry,
    new_span: usize,
    track_data: &mut TrackData,
    total_rows: usize,
    preferred_track: Option<usize>,
    from_bottom: bool,
) -> Option<usize> {
    let new_start = new_entry.time_ms;

    if new_start != track_data.last_compact_ms {
        track_data.last_compact_ms = new_start;
        compact_fixed_tracks(&mut track_data.tracks, new_start);
    }

    let sta_max = total_rows - new_span;
    let rows = &mut track_data.tracks;
    let new_end = new_entry.end_ms();

    if let Some(sta) = preferred_track.filter(|&sta| sta <= sta_max)
        && fixed_rows_free(rows, sta, new_span, new_start, new_end)
    {
        place_entry(rows, new_entry, sta, new_span);
        return Some(sta);
    }

    let mut candidates = 0..=sta_max;
    let found = if from_bottom {
        candidates
            .rev()
            .find(|&sta| fixed_rows_free(rows, sta, new_span, new_start, new_end))
    } else {
        candidates
            .find(|&sta| fixed_rows_free(rows, sta, new_span, new_start, new_end))
    };
    found.inspect(|&sta| place_entry(rows, new_entry, sta, new_span))
}

/// Push `entry` into every grating row its band covers.
fn place_entry(rows: &mut [Vec<TrackEntry>], entry: &TrackEntry, sta: usize, span: usize) {
    let mut placed = entry.clone();
    placed.row_start = sta as u16;
    placed.row_span = span as u16;
    for row in rows.iter_mut().take(sta + span).skip(sta) {
        row.push(placed.clone());
    }
}

/// Hash-based row for stacking mode (unconditional placement).
fn stack_row_for(entry: &TrackEntry, sta_max: usize) -> usize {
    let mut value = entry.danmaku_index.wrapping_mul(1_103_515_245usize);
    value ^= (entry.time_ms.max(0) as usize).rotate_left(11);
    value ^= (entry.paint_width.to_bits() as usize).rotate_left(5);
    value % (sta_max + 1).max(1)
}

/// Compact expired entries from fixed rows.
/// Fixed entries are kept while their time window is still open; entries are
/// not chained per row anymore (bands may share rows partially), so a plain
/// retain by end time is used.
fn compact_fixed_tracks(tracks: &mut [Vec<TrackEntry>], current_time_ms: i64) {
    for track in tracks.iter_mut() {
        track.retain(|existing| current_time_ms < existing.end_ms());
    }
}

/// A fixed band at `sta..sta+span` is free when no existing entry in those
/// rows overlaps in time.
fn fixed_rows_free(
    rows: &[Vec<TrackEntry>],
    sta: usize,
    span: usize,
    new_start: i64,
    new_end: i64,
) -> bool {
    rows.iter()
        .take(sta + span)
        .skip(sta)
        .all(|row| {
            row.iter()
                .all(|existing| existing.end_ms() <= new_start || existing.time_ms >= new_end)
        })
}

/// Whether a scroll entry placed at `sta` collides with any recent entry
/// whose vertical band intersects the candidate band.
fn scroll_recent_collide(
    new_entry: &TrackEntry,
    new_span: usize,
    sta: usize,
    recent: &VecDeque<TrackEntry>,
    view_width: f32,
) -> bool {
    let band_end = sta + new_span;
    recent.iter().any(|existing| {
        (existing.row_start as usize) < band_end
            && sta < (existing.row_start + existing.row_span) as usize
            && scroll_overlap_area(existing, new_entry, sta, view_width) > 0.0
    })
}

/// When the active set exceeds this budget, older entries are approximated
/// away (see OLD_ENTRY_CUTOFF_MS) to keep prepare time bounded during
/// sustained danmaku bursts. Only affects already-overloaded screens where
/// every placement is a soft overlap anyway; normal loads stay exact.
const ACTIVE_BUDGET: usize = 512;

/// During bursts, ignore entries that departed more than this long ago: they
/// are close to leaving the screen, so their collision weight is small and
/// overlapping them briefly is the least visible error.
const OLD_ENTRY_CUTOFF_MS: i64 = 1600;

/// Build the per-candidate-row crowding map for a scroll entry.
///
/// Mirrors BiliLocal's `calculate` (Running.cpp): for each existing entry f,
/// expand the band of candidate rows whose vertical band intersects f's, and
/// accumulate the 2D overlap area into each affected candidate. The
/// horizontal depth is computed once per f (it does not depend on the
/// candidate row); only the vertical overlap varies per row. `recent` is in
/// departure order, so the scan runs newest-first and can stop at the burst
/// cutoff.
fn build_scroll_crowding(
    new_entry: &TrackEntry,
    new_span: usize,
    recent: &VecDeque<TrackEntry>,
    sta_max: usize,
    view_width: f32,
) -> Vec<f32> {
    let mut thick = vec![0.0f32; sta_max + 1];
    let new_end = new_entry.time_ms + new_entry.duration_ms;

    // Burst mode: on a sustained-overload screen (a danmaku wall), entries
    // older than the cutoff are approximated away. They are close to leaving
    // the screen, every placement is a soft overlap anyway, and bounding the
    // active set keeps prepare time predictable. Normal loads stay exact.
    let cutoff = if recent.len() > ACTIVE_BUDGET {
        new_entry.time_ms.saturating_sub(OLD_ENTRY_CUTOFF_MS)
    } else {
        i64::MIN
    };

    for existing in recent.iter().rev() {
        if existing.time_ms < cutoff {
            break;
        }
        // Time guard: f already off screen, or f departs after s is done
        // (feeding order guarantees f.time_ms <= s.time_ms).
        if existing.end_ms() <= new_entry.time_ms || existing.time_ms >= new_end {
            continue;
        }
        let width = horizontal_depth(existing, new_entry, view_width);
        if width <= 0.0 {
            continue;
        }
        // Candidate rows whose band [sta, sta+span) intersects
        // [f.row_start, f.row_start+f.row_span):
        //   sta >= f.row_start - span + 1  and  sta <= f.row_start + f.row_span - 1
        let lo = ((existing.row_start as usize) + 1).saturating_sub(new_span);
        let hi = ((existing.row_start + existing.row_span) as usize)
            .saturating_sub(1)
            .min(sta_max);
        let f_top = existing.row_start as f32 * GRATING;
        let f_bottom = f_top + existing.paint_height;
        let s_height = new_entry.paint_height;
        for (sta, slot) in thick.iter_mut().enumerate().take(hi + 1).skip(lo) {
            let s_top = sta as f32 * GRATING;
            let v_overlap = f_bottom.min(s_top + s_height) - f_top.max(s_top);
            if v_overlap > 0.0 {
                *slot += v_overlap * width;
            }
        }
    }
    thick
}

// ---------------------------------------------------------------------------
// Collision detection (2D area, after BiliLocal Mode1::intersects)
// ---------------------------------------------------------------------------

/// 2D overlap area between an existing scroll entry `f` (already placed, at
/// its recorded rows) and a new entry `s` placed with its top at grating row
/// `s_row`. All horizontal positions are evaluated at `s`'s departure time
/// (the placement instant).
///
/// Vertical term: pixel overlap of the two bands.
/// Horizontal term (BiliLocal Mode1::intersects):
/// - If the two already overlap horizontally at the placement instant:
///   f slower or equal to s (s will keep closing in) → full min width;
///   f faster (escaping) → the current overlap width.
/// - If not yet overlapping: catch-up prediction. When s's leading edge
///   reaches the screen edge, would f still be in the way? R2L:
///   `o = f.right - f.speed * (s.left / s.speed)`; LR is the mirror image.
///   Positive o (clamped to both widths) is the collision depth.
#[inline]
fn scroll_overlap_area(f: &TrackEntry, s: &TrackEntry, s_row: usize, view_width: f32) -> f32 {
    let f_top = f.row_start as f32 * GRATING;
    let s_top = s_row as f32 * GRATING;
    let v_overlap = (f_top + f.paint_height).min(s_top + s.paint_height) - f_top.max(s_top);
    if v_overlap <= 0.0 {
        return 0.0;
    }
    if f.end_ms() <= s.time_ms || f.time_ms >= s.time_ms + s.duration_ms {
        return 0.0;
    }
    v_overlap * horizontal_depth(f, s, view_width)
}

/// Horizontal collision depth between an existing scroll entry `f` and a new
/// entry `s`, both evaluated at `s`'s departure instant. Invariant in the
/// candidate row, so the crowding builder computes it once per f.
#[inline]
fn horizontal_depth(f: &TrackEntry, s: &TrackEntry, view_width: f32) -> f32 {
    let f_left = entry_left_at(f, s.time_ms, view_width);
    let s_left = entry_left_at_start(s, view_width);
    let f_right = f_left + f.paint_width;
    let s_right = s_left + s.paint_width;

    let now_overlap = f_right.min(s_right) - f_left.max(s_left);
    if now_overlap > 0.0 {
        if f.step_x <= s.step_x {
            f.paint_width.min(s.paint_width)
        } else {
            now_overlap
        }
    } else {
        catch_up_width(f, s, f_left, f_right, s_left, s_right, view_width)
    }
}

/// Horizontal collision depth when the two do not overlap at the placement
/// instant (BiliLocal's `o = f.right - f.speed * s.left / s.speed`, with the
/// LR branch mirrored).
#[inline]
fn catch_up_width(
    f: &TrackEntry,
    s: &TrackEntry,
    f_left: f32,
    f_right: f32,
    s_left: f32,
    s_right: f32,
    view_width: f32,
) -> f32 {
    match s.danmaku_type {
        DanmakuType::ScrollRL => {
            if s.step_x <= 0.0 {
                return 0.0;
            }
            // Time until s's left edge reaches x=0.
            let t = s_left / s.step_x;
            let f_right_then = f_right - f.step_x * t;
            if f_right_then > 0.0 {
                f_right_then.min(f.paint_width).min(s.paint_width)
            } else {
                0.0
            }
        }
        DanmakuType::ScrollLR => {
            if s.step_x <= 0.0 {
                return 0.0;
            }
            // Time until s's right edge reaches x=view_width.
            let t = (view_width - s_right) / s.step_x;
            let f_left_then = f_left + f.step_x * t;
            let o = view_width - f_left_then;
            if o > 0.0 {
                o.min(f.paint_width).min(s.paint_width)
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

/// Boolean wrapper used by tests to assert collision semantics.
#[inline]
#[cfg(test)]
fn scroll_entries_collide(
    f: &TrackEntry,
    s: &TrackEntry,
    s_row: usize,
    view_width: f32,
) -> bool {
    scroll_overlap_area(f, s, s_row, view_width) > 0.0
}

#[inline]
fn entry_left_at_start(entry: &TrackEntry, view_width: f32) -> f32 {
    match entry.danmaku_type {
        DanmakuType::ScrollRL => view_width,
        DanmakuType::ScrollLR => -entry.paint_width,
        _ => 0.0,
    }
}

#[inline]
fn entry_left_at(entry: &TrackEntry, time_ms: i64, view_width: f32) -> f32 {
    if entry.danmaku_type == DanmakuType::ScrollLR {
        return entry_x_at(entry, time_ms, view_width);
    }

    let elapsed = (time_ms - entry.time_ms).max(0) as f32;

    if entry.step_x <= 0.0 {
        return view_width;
    }

    if elapsed >= entry.duration_ms as f32 {
        return -entry.paint_width;
    }

    let pos = view_width - elapsed * entry.step_x;
    pos.max(-entry.paint_width)
}

#[inline]
fn entry_x_at(entry: &TrackEntry, time_ms: i64, view_width: f32) -> f32 {
    let elapsed = (time_ms - entry.time_ms).max(0) as f32;
    if entry.step_x <= 0.0 {
        return match entry.danmaku_type {
            DanmakuType::ScrollRL => view_width,
            DanmakuType::ScrollLR => -entry.paint_width,
            _ => 0.0,
        };
    }
    match entry.danmaku_type {
        DanmakuType::ScrollRL => view_width - elapsed * entry.step_x,
        DanmakuType::ScrollLR => elapsed * entry.step_x - entry.paint_width,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dfm_core::model::DanmakuItem;

    fn calc_step_x(paint_width: f32, duration_ms: i64, view_width: f32) -> f32 {
        (view_width + paint_width) / duration_ms as f32
    }

    fn make_scroll_item(
        time_ms: i64,
        text: &str,
        paint_width: f32,
        danmaku_type: DanmakuType,
        duration_ms: i64,
        view_width: f32,
    ) -> DanmakuItem {
        let mut item = DanmakuItem::new(
            time_ms,
            text.into(),
            0xFFFFFFFF,
            25.0,
            danmaku_type,
            duration_ms,
        );
        item.paint_width = paint_width;
        item.paint_height = 30.0;
        item.step_x = calc_step_x(paint_width, duration_ms, view_width);
        item
    }

    fn make_fixed_item(
        time_ms: i64,
        text: &str,
        danmaku_type: DanmakuType,
        duration_ms: i64,
    ) -> DanmakuItem {
        let mut item = DanmakuItem::new(
            time_ms,
            text.into(),
            0xFFFFFFFF,
            25.0,
            danmaku_type,
            duration_ms,
        );
        item.paint_width = 100.0;
        item.paint_height = 30.0;
        item
    }

    /// Raw entry for collision-function tests: 30px tall, band [0, 3).
    fn raw_entry(
        time_ms: i64,
        duration_ms: i64,
        paint_width: f32,
        danmaku_type: DanmakuType,
        danmaku_index: usize,
        view_width: f32,
    ) -> TrackEntry {
        TrackEntry {
            time_ms,
            duration_ms,
            paint_width,
            paint_height: 30.0,
            step_x: calc_step_x(paint_width, duration_ms, view_width),
            danmaku_type,
            danmaku_index,
            row_start: 0,
            row_span: 3,
        }
    }

    #[test]
    fn test_first_item_placed_at_top() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut item = make_scroll_item(0, "test", 100.0, DanmakuType::ScrollRL, 5000, 1920.0);

        let (placed, _) = retainer.fix(&mut item, 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed);
        assert!(item.is_shown);
        assert!(
            (item.y - 2.0).abs() < 1.0,
            "first item y={} should be ~2.0",
            item.y
        );
    }

    #[test]
    fn test_same_time_items_different_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = [
            make_scroll_item(0, "first", 100.0, DanmakuType::ScrollRL, 5000, 1920.0),
            make_scroll_item(0, "second", 100.0, DanmakuType::ScrollRL, 5000, 1920.0),
        ];

        let (placed1, _) = retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed1);
        let first_y = items[0].y;

        let (placed2, _) = retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed2);
        assert!(
            items[1].y > first_y,
            "same-time items should be on different tracks: first_y={}, second_y={}",
            first_y,
            items[1].y
        );
    }

    #[test]
    fn test_non_overlapping_items_same_track() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = [
            make_scroll_item(0, "early", 100.0, DanmakuType::ScrollRL, 3000, 1920.0),
            make_scroll_item(10000, "late", 100.0, DanmakuType::ScrollRL, 3000, 1920.0),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        let first_y = items[0].y;

        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            (items[1].y - first_y).abs() < 1.0,
            "non-overlapping items should share track: first_y={}, second_y={}",
            first_y,
            items[1].y
        );
    }

    #[test]
    fn test_fixed_items_separate_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = [
            make_fixed_item(0, "top1", DanmakuType::FixTop, 3800),
            make_fixed_item(0, "top2", DanmakuType::FixTop, 3800),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        let first_y = items[0].y;

        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            items[1].y > first_y,
            "same-time fixed items should be on different tracks"
        );
    }

    #[test]
    fn test_fixed_expired_item_replaced() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = [
            make_fixed_item(0, "first", DanmakuType::FixTop, 3800),
            make_fixed_item(5000, "second", DanmakuType::FixTop, 3800),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        let first_y = items[0].y;

        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            (items[1].y - first_y).abs() < 1.0,
            "expired fixed item should be replaced: first_y={}, second_y={}",
            first_y,
            items[1].y
        );
    }

    #[test]
    fn test_scroll_collision_same_time() {
        let d1 = raw_entry(0, 5000, 100.0, DanmakuType::ScrollRL, 0, 1920.0);
        let d2 = raw_entry(0, 5000, 100.0, DanmakuType::ScrollRL, 1, 1920.0);
        assert!(scroll_entries_collide(&d1, &d2, 0, 1920.0));
        assert!(scroll_overlap_area(&d1, &d2, 0, 1920.0) > 0.0);
    }

    #[test]
    fn test_scroll_no_collision_far_apart() {
        let d1 = raw_entry(0, 3000, 100.0, DanmakuType::ScrollRL, 0, 1920.0);
        let d2 = raw_entry(10000, 3000, 100.0, DanmakuType::ScrollRL, 1, 1920.0);
        assert!(!scroll_entries_collide(&d1, &d2, 0, 1920.0));
        assert_eq!(scroll_overlap_area(&d1, &d2, 0, 1920.0), 0.0);
    }

    #[test]
    fn test_scroll_x_position() {
        let entry = raw_entry(0, 5000, 100.0, DanmakuType::ScrollRL, 0, 1920.0);
        let x0 = entry_x_at(&entry, 0, 1920.0);
        assert!((x0 - 1920.0).abs() < 1.0);
        let x5 = entry_x_at(&entry, 5000, 1920.0);
        assert!((x5 - (-100.0)).abs() < 1.0);
    }

    #[test]
    fn test_overflow_queues_item() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = [
            make_fixed_item(0, "a", DanmakuType::FixTop, 3800),
            make_fixed_item(0, "b", DanmakuType::FixTop, 3800),
        ];

        // view_height=60 → 6 grating rows; 30px×1.5 gap → span=5 rows.
        // Both candidate bands ([0,5) and [1,6)) overlap the placed entry.
        let (placed0, _) = retainer.fix(&mut items[0], 1920.0, 60.0, &flags, 1.0, false);
        assert!(placed0, "first item should be placed");

        let (placed1, _) = retainer.fix(&mut items[1], 1920.0, 60.0, &flags, 1.0, false);
        assert!(
            !placed1,
            "second item should be dropped when all bands are full (fixed never overlaps)"
        );
    }

    #[test]
    fn test_different_width_same_time_different_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = [
            make_scroll_item(0, "wide", 500.0, DanmakuType::ScrollRL, 5000, 1920.0),
            make_scroll_item(0, "narrow", 28.0, DanmakuType::ScrollRL, 5000, 1920.0),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            (items[0].y - items[1].y).abs() > 1.0,
            "different-width same-time items must be on different tracks: wide_y={}, narrow_y={}",
            items[0].y,
            items[1].y
        );
    }

    #[test]
    fn test_many_same_time_no_y_overlap() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);

        let texts: Vec<String> = (0..15).map(|i| format!("弹幕{}", i)).collect();
        let mut items: Vec<DanmakuItem> = texts
            .iter()
            .map(|text| make_scroll_item(1000, text, 150.0, DanmakuType::ScrollRL, 5000, 1920.0))
            .collect();

        let mut placed_ys = Vec::new();
        for item in &mut items {
            let (placed, _) = retainer.fix(item, 1920.0, 1080.0, &flags, 1.0, false);
            if placed {
                placed_ys.push(item.y);
            }
        }

        for i in 0..placed_ys.len() {
            for j in (i + 1)..placed_ys.len() {
                assert!(
                    (placed_ys[i] - placed_ys[j]).abs() > 1.0,
                    "items {} and {} share y={}",
                    i,
                    j,
                    placed_ys[i]
                );
            }
        }
    }

    /// Overload with overwrite enabled: the engine must soft-overlap at the
    /// least crowded row instead of dropping the item or displacing placed
    /// entries (the old stable-zone overwrite cleared whole tracks; the new
    /// engine never displaces except for `is_me`).
    #[test]
    fn test_scroll_overload_soft_overlaps_without_displacing() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(0.0, 0.0);
        let view_width = 640.0;
        let view_height = 150.0; // 15 rows; 30px span=3 → five items fill it.
        let mut items: Vec<DanmakuItem> = (0..6)
            .map(|i| {
                let mut item = make_scroll_item(
                    0,
                    &format!("dense-{i}"),
                    160.0,
                    DanmakuType::ScrollRL,
                    5000,
                    view_width,
                );
                item.index = i;
                item
            })
            .collect();

        let mut displaced_total: Vec<usize> = Vec::new();
        for item in &mut items {
            let (placed, displaced) = retainer.fix_with_options(
                item,
                view_width,
                view_height,
                &flags,
                1.0,
                false,
                false,
                true,
            );
            assert!(placed, "overloaded scroll danmaku must soft-overlap, not drop");
            displaced_total.extend(displaced);
        }
        assert!(
            displaced_total.is_empty(),
            "soft overlap must not displace placed entries, got {displaced_total:?}"
        );
        assert!(
            items.iter().all(|item| item.is_shown),
            "all six items must be placed"
        );
    }

    /// Overload with overwrite disabled (user turned it off): drop instead
    /// of soft-overlapping.
    #[test]
    fn test_scroll_overload_without_overwrite_drops() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(0.0, 0.0);
        let view_width = 640.0;
        let view_height = 150.0;
        let mut last_placed = false;
        for i in 0..6 {
            let mut item = make_scroll_item(
                0,
                &format!("drop-{i}"),
                160.0,
                DanmakuType::ScrollRL,
                5000,
                view_width,
            );
            let (placed, _) = retainer.fix_with_options(
                &mut item,
                view_width,
                view_height,
                &flags,
                1.0,
                false,
                false,
                false,
            );
            if i < 5 {
                assert!(placed, "item {i} fits before overload");
            } else {
                last_placed = placed;
            }
        }
        assert!(!last_placed, "6th item must drop when overwrite is off");
    }

    #[test]
    fn test_chain_queue_fixed_items() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items: Vec<DanmakuItem> = (0..4)
            .map(|i| {
                let mut item = make_fixed_item(0, &format!("top{}", i), DanmakuType::FixTop, 3800);
                item.index = i as u32;
                item
            })
            .collect();

        // view_height=60 → 6 rows; 30px×1.5 → span=5. First item takes a
        // band; the remaining candidates all intersect it → dropped.
        for i in 0..items.len() {
            items[i].index = i as u32;
            let (placed, _) = retainer.fix(&mut items[i], 1920.0, 60.0, &flags, 1.0, false);
            if i == 0 {
                assert!(placed, "item 0 should be placed (first on empty band)");
                assert_eq!(items[0].time_ms, 0);
            } else {
                assert!(
                    !placed,
                    "item {} should be dropped when all bands are full",
                    i
                );
            }
        }
    }

    #[test]
    fn test_staggered_items_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);

        let mut items: Vec<DanmakuItem> = (0..10)
            .map(|i| {
                make_scroll_item(
                    i * 100,
                    &format!("item_{}", i),
                    150.0,
                    DanmakuType::ScrollRL,
                    5000,
                    1920.0,
                )
            })
            .collect();

        for item in &mut items {
            retainer.fix(item, 1920.0, 1080.0, &flags, 1.0, false);
        }

        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let time_diff = (items[j].time_ms - items[i].time_ms).abs();
                if time_diff < 5000 {
                    let y_diff = (items[i].y - items[j].y).abs();
                    if y_diff < 1.0 && items[i].y >= 0.0 && items[j].y >= 0.0 {
                        println!(
                            "Same track: item_{} (t={}) and item_{} (t={}), y={}",
                            i, items[i].time_ms, j, items[j].time_ms, items[i].y
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_fixed_top_overlap_no_visual_overlap() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items: Vec<DanmakuItem> = (0..5)
            .map(|i| make_fixed_item(0, &format!("top{}", i), DanmakuType::FixTop, 3800))
            .collect();

        for i in 0..items.len() {
            let (_, displaced) = retainer.fix(&mut items[i], 1920.0, 1080.0, &flags, 1.0, false);
            for &d in &displaced {
                items[d].is_filtered = true;
                items[d].filter_param = 99;
            }
        }

        let mut visible_ys = Vec::new();
        for item in &items {
            if item.is_shown && !item.is_filtered {
                visible_ys.push(item.y);
            }
        }

        for i in 0..visible_ys.len() {
            for j in (i + 1)..visible_ys.len() {
                assert!(
                    (visible_ys[i] - visible_ys[j]).abs() > 1.0,
                    "visible items {} and {} share y={}, causing visual overlap",
                    i,
                    j,
                    visible_ys[i]
                );
            }
        }
    }

    #[test]
    fn test_fix_bottom_overflow_queues_correctly() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items: Vec<DanmakuItem> = (0..4)
            .map(|i| {
                let mut item =
                    make_fixed_item(0, &format!("bottom{}", i), DanmakuType::FixBottom, 3800);
                item.index = i as u32;
                item
            })
            .collect();

        // view_height=60 → 6 rows; span=5. First item placed; rest dropped.
        for i in 0..items.len() {
            items[i].index = i as u32;
            let (placed, _) = retainer.fix(&mut items[i], 1920.0, 60.0, &flags, 1.0, false);
            if i == 0 {
                assert!(placed, "item 0 should be placed");
                assert_eq!(items[0].time_ms, 0);
            } else {
                assert!(
                    !placed,
                    "item {} should be dropped when all bands are full",
                    i
                );
            }
        }
    }

    /// Bottom danmaku align their bottom edge to the bottom of the full view
    /// height (minus margin).
    #[test]
    fn test_fix_bottom_aligns_to_bottom_edge() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.0);
        let mut item = make_fixed_item(0, "bottom", DanmakuType::FixBottom, 3800);

        let (placed, _) = retainer.fix(&mut item, 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed);
        let bottom_edge = item.y + item.paint_height;
        assert!(
            (bottom_edge - 1078.0).abs() <= 1.0,
            "bottom edge should sit at view_height - margin = 1078, got {bottom_edge}"
        );
    }

    /// display_area shrinks the scroll row pool only: fixed danmaku keep the
    /// full-height geometry (top pinned to the screen top, bottom pinned to
    /// the screen bottom).
    #[test]
    fn test_display_area_does_not_move_fixed_danmaku() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.0);
        let mut bottom = make_fixed_item(0, "bottom", DanmakuType::FixBottom, 3800);
        let mut top = make_fixed_item(1, "top", DanmakuType::FixTop, 3800);

        let (placed_b, _) = retainer.fix(&mut bottom, 1920.0, 1080.0, &flags, 0.25, false);
        assert!(placed_b);
        assert!(
            (bottom.y + bottom.paint_height - 1078.0).abs() <= 1.0,
            "bottom danmaku must stay pinned to the screen bottom at display_area=0.25, got {}",
            bottom.y + bottom.paint_height
        );

        let (placed_t, _) = retainer.fix(&mut top, 1920.0, 1080.0, &flags, 0.25, false);
        assert!(placed_t);
        assert!(
            (top.y - 2.0).abs() <= 1.0,
            "top danmaku must stay pinned to the screen top at display_area=0.25, got {}",
            top.y
        );
    }

    #[test]
    fn test_long_danmaku_catches_short() {
        let short = raw_entry(0, 8000, 50.0, DanmakuType::ScrollRL, 0, 1920.0);
        let long = raw_entry(1000, 8000, 400.0, DanmakuType::ScrollRL, 1, 1920.0);
        assert!(
            scroll_entries_collide(&short, &long, 0, 1920.0),
            "long danmaku starting later should catch up to short danmaku on same track"
        );
    }

    #[test]
    fn test_no_false_positive_at_start() {
        let short = raw_entry(0, 8000, 50.0, DanmakuType::ScrollRL, 0, 1920.0);
        let long = raw_entry(1000, 8000, 400.0, DanmakuType::ScrollRL, 1, 1920.0);
        let short_left = entry_left_at(&short, 1000, 1920.0);
        let short_right = short_left + short.paint_width;
        let long_left = entry_left_at(&long, 1000, 1920.0);
        assert!(
            long_left >= short_right,
            "at d2 start: long.left={} should be >= short.right={} (no overlap yet)",
            long_left,
            short_right
        );
    }

    #[test]
    fn test_catch_up_at_end() {
        let short = raw_entry(0, 8000, 50.0, DanmakuType::ScrollRL, 0, 1920.0);
        let long = raw_entry(1000, 8000, 400.0, DanmakuType::ScrollRL, 1, 1920.0);
        let short_left = entry_left_at(&short, 8000, 1920.0);
        let short_right = short_left + short.paint_width;
        let long_left = entry_left_at(&long, 8000, 1920.0);
        assert!(
            long_left < short_right,
            "at d1 end: long.left={} should be < short.right={} (catch-up happened)",
            long_left,
            short_right
        );
    }

    #[test]
    fn test_no_collision_when_far_apart_in_time() {
        let short = raw_entry(0, 8000, 50.0, DanmakuType::ScrollRL, 0, 1920.0);
        let long = raw_entry(7000, 8000, 400.0, DanmakuType::ScrollRL, 1, 1920.0);
        assert!(
            !scroll_entries_collide(&short, &long, 0, 1920.0),
            "long danmaku starting 7s later should not catch short (short ends at 8s, only 1s overlap window not enough)"
        );
    }

    #[test]
    fn test_long_short_different_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.15);
        let mut short_item = make_scroll_item(0, "短", 50.0, DanmakuType::ScrollRL, 8000, 1920.0);
        let mut long_item = make_scroll_item(
            1000,
            "很长很长的弹幕内容在这里",
            400.0,
            DanmakuType::ScrollRL,
            8000,
            1920.0,
        );

        retainer.fix(&mut short_item, 1920.0, 1080.0, &flags, 1.0, false);
        retainer.fix(&mut long_item, 1920.0, 1080.0, &flags, 1.0, false);

        assert!(
            (short_item.y - long_item.y).abs() > 1.0,
            "long danmaku (y={}) should be on different track from short (y={}) to avoid catch-up",
            long_item.y,
            short_item.y
        );
    }

    #[test]
    fn test_check_hit_direction_rtl() {
        let d1 = raw_entry(0, 5000, 100.0, DanmakuType::ScrollRL, 0, 1920.0);
        let d2 = raw_entry(500, 5000, 100.0, DanmakuType::ScrollRL, 1, 1920.0);

        assert!(
            !scroll_entries_collide(&d1, &d2, 0, 1920.0),
            "same-speed danmaku 500ms apart should NOT collide (they maintain distance)"
        );

        let d1_fast = raw_entry(0, 8000, 50.0, DanmakuType::ScrollRL, 0, 1920.0);
        let d2_slow = raw_entry(1000, 8000, 400.0, DanmakuType::ScrollRL, 1, 1920.0);

        let t_start = 1000;
        let d1_right_at_start = entry_left_at(&d1_fast, t_start, 1920.0) + d1_fast.paint_width;
        let d2_left_at_start = entry_left_at(&d2_slow, t_start, 1920.0);
        assert!(
            d2_left_at_start >= d1_right_at_start,
            "at d2 start: d2.left={} should be >= d1.right={} (no overlap yet, d2 just entered)",
            d2_left_at_start,
            d1_right_at_start
        );

        let t_end = 8000;
        let d1_right_at_end = entry_left_at(&d1_fast, t_end, 1920.0) + d1_fast.paint_width;
        let d2_left_at_end = entry_left_at(&d2_slow, t_end, 1920.0);
        assert!(
            d2_left_at_end < d1_right_at_end,
            "at d1 end: d2.left={} should be < d1.right={} (long danmaku caught up!)",
            d2_left_at_end,
            d1_right_at_end
        );

        assert!(
            scroll_entries_collide(&d1_fast, &d2_slow, 0, 1920.0),
            "long fast danmaku should collide with short slow danmaku"
        );
    }

    /// Core regression for the mixed-font-size overlap bug: danmaku of
    /// heights 22/30/43px (Bilibili 18/25/36pt text) placed at the same
    /// instant must never overlap vertically. With the old per-item track
    /// heights, a 43px item on track N and a 22px item on track N+1
    /// overlapped by ~(43-30)px whenever tracks were dense.
    #[test]
    fn test_mixed_font_sizes_no_vertical_overlap() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.0);
        let heights = [22.0, 30.0, 43.0];
        let mut items: Vec<DanmakuItem> = (0..24)
            .map(|i| {
                let mut item = make_scroll_item(
                    1000,
                    &format!("混排{i}"),
                    150.0,
                    DanmakuType::ScrollRL,
                    5000,
                    1920.0,
                );
                item.paint_height = heights[i % 3];
                item
            })
            .collect();

        let mut placed: Vec<(f32, f32)> = Vec::new();
        for item in &mut items {
            let (ok, _) = retainer.fix(item, 1920.0, 1080.0, &flags, 1.0, false);
            assert!(ok, "24 mixed-size items must fit in 108 grating rows");
            placed.push((item.y, item.paint_height));
        }

        for i in 0..placed.len() {
            for j in (i + 1)..placed.len() {
                let (y1, h1) = placed[i];
                let (y2, h2) = placed[j];
                let overlap = (y1 + h1).min(y2 + h2) - y1.max(y2);
                assert!(
                    overlap <= 0.0,
                    "items {i} and {j} vertically overlap by {overlap}px (y={y1}/{y2}, h={h1}/{h2})"
                );
            }
        }
    }

    /// `is_me` claims the top band when the screen is full, displacing the
    /// entries occupying it (the only displacement path).
    #[test]
    fn test_is_me_claims_top_rows_when_full() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(0.0, 0.0);
        let view_width = 640.0;
        let view_height = 150.0; // 15 rows; span=3 → 5 items fill it.

        for i in 0..5 {
            let mut item = make_scroll_item(
                0,
                &format!("fill-{i}"),
                160.0,
                DanmakuType::ScrollRL,
                5000,
                view_width,
            );
            item.index = i;
            let (placed, _) = retainer.fix_with_options(
                &mut item,
                view_width,
                view_height,
                &flags,
                1.0,
                false,
                false,
                true,
            );
            assert!(placed);
        }

        let mut mine = make_scroll_item(
            0,
            "mine",
            160.0,
            DanmakuType::ScrollRL,
            5000,
            view_width,
        );
        mine.index = 99;
        let (placed, displaced) = retainer.fix_with_options(
            &mut mine,
            view_width,
            view_height,
            &flags,
            1.0,
            true,
            false,

            true,
        );
        assert!(placed, "is_me must be placed even on a full screen");
        assert!((mine.y - 0.0).abs() < 1.0, "is_me claims the top band, y={}", mine.y);
        assert!(
            displaced.contains(&0),
            "is_me must displace the entry occupying the top band, got {displaced:?}"
        );
    }
}
