//! Vendored into DyPlayer (crates/dyplayer-danmaku) from liberika v0.2.1
//! (https://github.com/AimesSoft/liberika, MPL-2.0), where it lived as
//! `erika/src/danmaku/outline.rs`. 已偏离上游：三档阶梯改连续倍率、
//! 半径分数化（渲染 floor 硬环 + 衰减外环）；调参常量仍集中于此。

//! Erika danmaku outline widths and raster quantization.
//!
//! Layout and rendering must use the same resolved width so collision bounds
//! match the glyphs drawn on screen. Keep every tuning constant here.

const BASE_WIDTH_RATIO: f32 = 0.06;
const BASE_WIDTH_MIN_PX: f32 = 1.0;
const BASE_WIDTH_MAX_PX: f32 = 2.6;
/// 描边滑杆值→宽度倍率：2 = 1×base（旧 normal 档），4 = 2×base（旧 thick 档）。
const OUTLINE_VALUE_SCALE: f32 = 0.5;
const MAX_RASTER_RADIUS_PX: f32 = 16.0;

fn base_width(font_size: f32) -> f32 {
    (font_size * BASE_WIDTH_RATIO).clamp(BASE_WIDTH_MIN_PX, BASE_WIDTH_MAX_PX)
}

/// 描边滑杆值（0-4 连续）→ 物理像素宽（分数，渲染侧量化到 0.25px）。
pub(super) fn resolve_width_px(font_size: f32, value: f32) -> f32 {
    if !value.is_finite() || value <= 0.0 {
        return 0.0;
    }
    (base_width(font_size) * value * OUTLINE_VALUE_SCALE).min(MAX_RASTER_RADIUS_PX)
}

/// 碰撞外扩取 ceil：分数半径渲染为 floor 硬环 + 1px 衰减外环，
/// 实际外包围 = ceil(width)。
pub(super) fn ceil_radius(width_px: f32) -> u16 {
    if !width_px.is_finite() {
        return 0;
    }
    width_px.ceil().clamp(0.0, MAX_RASTER_RADIUS_PX) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_matches_legacy_anchor_points() {
        for font_size in [20.0f32, 25.0, 30.0, 40.0, 60.0] {
            let base =
                (font_size * BASE_WIDTH_RATIO).clamp(BASE_WIDTH_MIN_PX, BASE_WIDTH_MAX_PX);
            assert_eq!(resolve_width_px(font_size, 2.0), base, "旧 normal 档");
            assert_eq!(resolve_width_px(font_size, 4.0), base * 2.0, "旧 thick 档");
        }
    }

    #[test]
    fn small_values_proportional_and_monotonic() {
        assert!(resolve_width_px(30.0, 0.25) > 0.0, "小值不再死区");
        assert!(resolve_width_px(30.0, 0.25) < resolve_width_px(30.0, 0.5));
    }

    #[test]
    fn clamps_and_rejects_non_finite() {
        assert_eq!(resolve_width_px(30.0, 0.0), 0.0);
        assert_eq!(resolve_width_px(30.0, -1.0), 0.0);
        assert_eq!(resolve_width_px(30.0, f32::NAN), 0.0);
        assert_eq!(resolve_width_px(30.0, 1_000.0), MAX_RASTER_RADIUS_PX);
        assert_eq!(ceil_radius(f32::NAN), 0);
        assert_eq!(ceil_radius(f32::INFINITY), 0);
        assert_eq!(ceil_radius(1.2), 2);
        assert_eq!(ceil_radius(2.0), 2);
        assert_eq!(ceil_radius(1_000.0), MAX_RASTER_RADIUS_PX as u16);
    }
}