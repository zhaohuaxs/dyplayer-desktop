//! 浏览域自绘矢量图标库（Glyph + painter 几何绘制）。
//!
//! 浏览域图标（folder / cloud / server / star / history / search 等）在
//! modernz-icons.ttf 中缺失（该字体全部为播放控制类图标），以 painter
//! 矢量 glyph 补齐。全部比例化绘制：坐标按 rect 比例、线宽随尺寸缩放
//! （clamp 上下限），同一 glyph 在 11–40px 区间笔画比例稳定。

use egui::{Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};

use crate::browser::SourceKind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Glyph {
    Home,
    Folder,
    Cloud,
    Server,
    Star,
    History,
    Search,
    Play,
    Check,
    Plus,
    Movie,
    Pencil,
    Grid,
    Settings,
}

/// 媒体源类型 → 图标（侧栏源行 / 源卡片）。
pub(crate) fn source_kind_glyph(kind: SourceKind) -> Glyph {
    match kind {
        SourceKind::Local => Glyph::Folder,
        SourceKind::WebDav => Glyph::Cloud,
        SourceKind::Smb => Glyph::Server,
    }
}

/// 在 rect（约方形）内居中绘制 glyph，颜色统一 tint。
pub(crate) fn paint_glyph(p: &Painter, glyph: Glyph, rect: Rect, color: Color32) {
    match glyph {
        Glyph::Home => paint_home(p, rect, color),
        Glyph::Folder => paint_folder(p, rect, color),
        Glyph::Cloud => paint_cloud(p, rect, color),
        Glyph::Server => paint_server(p, rect, color),
        Glyph::Star => {
            let r = rect.size().min_elem() * 0.5;
            paint_star(p, rect.center(), r, color);
        }
        Glyph::History => paint_history(p, rect, color),
        Glyph::Search => paint_search(p, rect, color),
        Glyph::Play => {
            let r = rect.size().min_elem() * 0.5;
            paint_play(p, rect.center(), r, color);
        }
        Glyph::Check => {
            let r = rect.size().min_elem() * 0.5;
            paint_check(p, rect.center(), r, color);
        }
        Glyph::Plus => {
            let r = rect.size().min_elem() * 0.5;
            paint_plus(p, rect.center(), r, color);
        }
        Glyph::Movie => paint_movie(p, rect, color),
        Glyph::Pencil => paint_pencil(p, rect, color),
        Glyph::Grid => paint_grid(p, rect, color),
        Glyph::Settings => paint_settings(p, rect, color),
    }
}

/// 设置齿轮：外圈 + 内圈 + 8 齿短块（线宽随尺寸缩放）。
fn paint_settings(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let r = rect.size().min_elem() * 0.30;
    let sw = (r * 0.30).clamp(1.2, 2.6);
    p.circle_stroke(c, r, Stroke::new(sw, color));
    p.circle_stroke(c, r * 0.42, Stroke::new(sw, color));
    for k in 0..8 {
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let dir = vec2(a.cos(), a.sin());
        p.line_segment(
            [c + dir * (r * 0.92), c + dir * (r * 1.34)],
            Stroke::new(sw * 1.5, color),
        );
    }
}

fn paint_grid(p: &Painter, rect: Rect, color: Color32) {
    let (x0, y0) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    let (cw, ch) = (w * 0.42, h * 0.42);
    let gap_x = w * 0.16;
    let gap_y = h * 0.16;
    let positions = [
        (x0 + w * 0.08, y0 + h * 0.08),
        (x0 + w * 0.08 + cw + gap_x, y0 + h * 0.08),
        (x0 + w * 0.08, y0 + h * 0.08 + ch + gap_y),
        (x0 + w * 0.08 + cw + gap_x, y0 + h * 0.08 + ch + gap_y),
    ];
    for (px, py) in positions {
        p.rect_filled(
            Rect::from_min_size(pos2(px, py), vec2(cw, ch)),
            CornerRadius::same(2),
            color,
        );
    }
}

fn paint_folder(p: &Painter, rect: Rect, color: Color32) {
    let (x0, y0) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    // 标签与主体顶边无缝衔接（标签底 0.30h > 主体顶 0.28h）。
    p.rect_filled(
        Rect::from_min_size(pos2(x0 + w * 0.10, y0 + h * 0.16), vec2(w * 0.34, h * 0.14)),
        CornerRadius::same(2),
        color,
    );
    p.rect_filled(
        Rect::from_min_size(pos2(x0 + w * 0.06, y0 + h * 0.28), vec2(w * 0.88, h * 0.58)),
        CornerRadius::same(3),
        color,
    );
}

fn paint_cloud(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let r = rect.size().min_elem() * 0.5;
    // 三圆底部与矩形底边共线（baseline），消除下缘错位/缺口。
    let baseline = c.y + r * 0.42;
    let left = c.x - r * 0.84;
    let right = c.x + r * 0.76;
    p.circle_filled(pos2(c.x - r * 0.10, c.y - r * 0.08), r * 0.50, color);
    p.circle_filled(pos2(c.x - r * 0.50, c.y + r * 0.08), r * 0.34, color);
    p.circle_filled(pos2(c.x + r * 0.42, c.y + r * 0.08), r * 0.34, color);
    p.rect_filled(
        Rect::from_min_max(pos2(left, c.y + r * 0.08), pos2(right, baseline)),
        CornerRadius::same(2),
        color,
    );
}

fn paint_server(p: &Painter, rect: Rect, color: Color32) {
    let (x0, y0) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    for i in 0..2 {
        let unit = Rect::from_min_size(
            pos2(x0 + w * 0.08, y0 + h * (0.14 + i as f32 * 0.5)),
            vec2(w * 0.84, h * 0.34),
        );
        p.rect_filled(unit, CornerRadius::same(2), color);
        p.circle_filled(
            pos2(unit.left() + unit.width() * 0.14, unit.center().y),
            h * 0.05,
            fade(color, 0.35),
        );
    }
}

fn paint_star(p: &Painter, center: Pos2, r: f32, color: Color32) {
    // 五角星是凹多边形，不能整个交给 convex_polygon（三角剖分错误致畸）。
    // 改为从中心向每对相邻顶点做三角扇：每片都是凸三角形，正确填满星形。
    let mut pts = [Pos2::ZERO; 10];
    for (i, pt) in pts.iter_mut().enumerate() {
        let rad = if i % 2 == 0 { r } else { r * 0.42 };
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        *pt = pos2(center.x + rad * a.cos(), center.y + rad * a.sin());
    }
    for i in 0..10 {
        p.add(egui::Shape::convex_polygon(
            vec![center, pts[i], pts[(i + 1) % 10]],
            color,
            Stroke::NONE,
        ));
    }
}

fn paint_history(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let r = rect.size().min_elem() * 0.36;
    let sw = (r * 0.28).clamp(1.4, 2.6);
    p.circle_stroke(c, r, Stroke::new(sw, color));
    p.line_segment([c, pos2(c.x, c.y - r * 0.62)], Stroke::new(sw, color));
    p.line_segment(
        [c, pos2(c.x + r * 0.52, c.y + r * 0.18)],
        Stroke::new(sw, color),
    );
}

fn paint_search(p: &Painter, rect: Rect, color: Color32) {
    let s = rect.size().min_elem();
    let r = s * 0.30;
    let sw = (r * 0.30).clamp(1.4, 2.6);
    // 圆心左上偏置，柄自圆周起（消除柄与圆脱节）。
    let c = pos2(rect.left() + s * 0.42, rect.top() + s * 0.42);
    p.circle_stroke(c, r, Stroke::new(sw, color));
    let d = r * std::f32::consts::FRAC_1_SQRT_2;
    p.line_segment(
        [
            pos2(c.x + d, c.y + d),
            pos2(c.x + d + r * 0.75, c.y + d + r * 0.75),
        ],
        Stroke::new(sw, color),
    );
}

fn paint_home(p: &Painter, rect: Rect, color: Color32) {
    let (x0, y0) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    // 屋顶三角底边（0.14–0.86w）略宽于房体（0.20–0.80w），
    // 且屋顶底 0.48h 覆盖房体顶 0.44h —— 消除屋顶与房体错位缝隙。
    p.add(egui::Shape::convex_polygon(
        vec![
            pos2(x0 + w * 0.5, y0 + h * 0.08),
            pos2(x0 + w * 0.86, y0 + h * 0.48),
            pos2(x0 + w * 0.14, y0 + h * 0.48),
        ],
        color,
        Stroke::NONE,
    ));
    p.rect_filled(
        Rect::from_min_size(pos2(x0 + w * 0.20, y0 + h * 0.44), vec2(w * 0.60, h * 0.48)),
        CornerRadius::same(2),
        color,
    );
}

fn paint_play(p: &Painter, center: Pos2, r: f32, color: Color32) {
    // 光学补偿：三角形质心右移，视觉居中。
    let cx = center.x + r * 0.14;
    p.add(egui::Shape::convex_polygon(
        vec![
            pos2(cx - r * 0.62, center.y - r),
            pos2(cx - r * 0.62, center.y + r),
            pos2(cx + r, center.y),
        ],
        color,
        Stroke::NONE,
    ));
}

fn paint_check(p: &Painter, center: Pos2, r: f32, color: Color32) {
    let sw = (r * 0.30).clamp(1.2, 2.2);
    p.line_segment(
        [
            pos2(center.x - r * 0.75, center.y + r * 0.05),
            pos2(center.x - r * 0.15, center.y + r * 0.62),
        ],
        Stroke::new(sw, color),
    );
    p.line_segment(
        [
            pos2(center.x - r * 0.15, center.y + r * 0.62),
            pos2(center.x + r * 0.8, center.y - r * 0.6),
        ],
        Stroke::new(sw, color),
    );
}

fn paint_plus(p: &Painter, center: Pos2, r: f32, color: Color32) {
    let sw = (r * 0.28).clamp(1.2, 2.2);
    p.line_segment(
        [pos2(center.x - r, center.y), pos2(center.x + r, center.y)],
        Stroke::new(sw, color),
    );
    p.line_segment(
        [pos2(center.x, center.y - r), pos2(center.x, center.y + r)],
        Stroke::new(sw, color),
    );
}

fn paint_movie(p: &Painter, rect: Rect, color: Color32) {
    let (x0, y0) = (rect.left(), rect.top());
    let (w, h) = (rect.width(), rect.height());
    let sw = (w * 0.08).clamp(1.4, 2.2);
    // 圆角矩形外框（视频文件轮廓）。
    p.rect_stroke(
        Rect::from_min_size(pos2(x0 + w * 0.12, y0 + h * 0.1), vec2(w * 0.76, h * 0.8)),
        CornerRadius::same(3),
        Stroke::new(sw, color),
        StrokeKind::Inside,
    );
    // 中央播放三角形（光学补偿：质心右移）。
    let cx = x0 + w * 0.5;
    let cy = y0 + h * 0.5;
    let r = w * 0.16;
    p.add(egui::Shape::convex_polygon(
        vec![
            pos2(cx - r * 0.62, cy - r),
            pos2(cx - r * 0.62, cy + r),
            pos2(cx + r, cy),
        ],
        fade(color, 0.7),
        Stroke::NONE,
    ));
}

/// 铅笔（编辑）：矩形笔杆 + 三角笔尖，全比例化（线宽/杆宽随尺寸缩放）。
fn paint_pencil(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.size().min_elem() * 0.5;
    let sw = (s * 0.24).clamp(1.2, 2.2);
    let dir = vec2(std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2);
    let nrm = vec2(std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    let half = s * 0.22;
    let tail = c + dir * (s * 0.70);
    let neck = c + dir * (s * 0.05);
    let tip = c - dir * (s * 0.72);
    // 笔杆两侧边 + 尾端封口。
    p.line_segment([tail + nrm * half, neck + nrm * half], Stroke::new(sw, color));
    p.line_segment([tail - nrm * half, neck - nrm * half], Stroke::new(sw, color));
    p.line_segment([tail + nrm * half, tail - nrm * half], Stroke::new(sw, color));
    // 笔尖两条边汇聚至尖点。
    p.line_segment([neck + nrm * half, tip], Stroke::new(sw, color));
    p.line_segment([neck - nrm * half, tip], Stroke::new(sw, color));
}

/// 颜色透明度缩放（保持 RGB，乘 alpha；自绘层统一用）。
pub(crate) fn fade(c: Color32, a: f32) -> Color32 {
    let a = a.clamp(0.0, 1.0);
    Color32::from_rgba_unmultiplied(
        c.r(),
        c.g(),
        c.b(),
        (c.a() as f32 * a).round().clamp(0.0, 255.0) as u8,
    )
}

/// 颜色插值（RGBA 各通道线性混合；自绘层统一用）。
pub(crate) fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgba_unmultiplied(
        ch(a.r(), b.r()),
        ch(a.g(), b.g()),
        ch(a.b(), b.b()),
        ch(a.a(), b.a()),
    )
}

/// 超长文本截断（按字符数，尾部加省略号）。
pub(crate) fn truncate_tail(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        let cut: String = s.chars().take(max_chars - 1).collect();
        format!("{cut}…")
    } else {
        s.to_owned()
    }
}

/// 左对齐垂直居中的截断文本（超宽时逐字符收缩 + 省略号）。
/// browse_ui / player_app 播放队列行同工艺收敛于此。
pub(crate) fn paint_left_center_truncated(
    p: &Painter,
    pos: Pos2,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) {
    let gal = p.layout_no_wrap(text.to_owned(), font.clone(), color);
    if gal.size().x <= max_w {
        p.galley(pos2(pos.x, pos.y - gal.size().y / 2.0), gal, color);
        return;
    }
    let mut kept = String::new();
    for ch in text.chars() {
        let mut test = kept.clone();
        test.push(ch);
        test.push('…');
        let g = p.layout_no_wrap(test, font.clone(), color);
        if g.size().x > max_w {
            break;
        }
        kept.push(ch);
    }
    let gal = p.layout_no_wrap(format!("{kept}…"), font, color);
    p.galley(pos2(pos.x, pos.y - gal.size().y / 2.0), gal, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_glyph_mapping() {
        assert_eq!(source_kind_glyph(SourceKind::Local), Glyph::Folder);
        assert_eq!(source_kind_glyph(SourceKind::WebDav), Glyph::Cloud);
        assert_eq!(source_kind_glyph(SourceKind::Smb), Glyph::Server);
    }

    #[test]
    fn fade_scales_alpha_only() {
        let c = Color32::from_rgb(200, 100, 50);
        // from_rgba_unmultiplied 构造时预乘：alpha=255 时 RGB 不变。
        let full = fade(c, 1.0);
        assert_eq!((full.r(), full.g(), full.b(), full.a()), (200, 100, 50, 255));
        assert_eq!(fade(c, 0.0).a(), 0);
        assert_eq!(fade(c, 2.0).a(), 255);
        let half = fade(c, 0.5);
        assert_eq!(half.a(), 128);
        // 预乘语义：RGB 通道随 alpha 等比（≈ 半值，取整容差 ±1）。
        assert!((half.r() as i32 - 100).abs() <= 1);
        assert!((half.g() as i32 - 50).abs() <= 1);
        assert!((half.b() as i32 - 25).abs() <= 1);
    }

    #[test]
    fn mix_interpolates_channels() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(255, 255, 255);
        let m = mix(a, b, 0.5);
        assert_eq!((m.r(), m.g(), m.b()), (128, 128, 128));
    }

    #[test]
    fn truncate_tail_limits_chars() {
        assert_eq!(truncate_tail("短", 5), "短");
        assert_eq!(truncate_tail("abcdefghij", 5), "abcd…");
        assert_eq!(truncate_tail("一二三四五", 3), "一二…");
    }
}