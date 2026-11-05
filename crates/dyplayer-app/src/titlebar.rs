//! 应用级自定义标题栏 + 边缘 resize。
//! 窗口本体经 fs_patch::prime_borderless 常态无边框（WS_THICKFRAME +
//! WM_NCCALCSIZE，**勿用 with_decorations(false)**——Win10 虚拟机上 winit
//! 剥光样式会致 DWM 呈现失效、客户区全黑）。
//! 交互：整栏拖动（StartDrag）/ 双击最大化 / 右侧三钮 / 边缘 8 向 resize
//! （BeginResize；子类 NCHITTEST 恒 HTCLIENT，本模块是唯一 resize 路径）。

use egui::{Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

pub const HEIGHT: f32 = 36.0;
/// 边缘 resize 感应宽度（逻辑像素）。
const EDGE: f32 = 6.0;
const CORNER: f32 = 14.0;

pub struct TitleBarStyle {
    pub bg: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub hover: Color32,
    pub danger: Color32,
}

impl TitleBarStyle {
    /// 播放页常驻深色（视频区黑底、空态也黑，与全局明暗主题无关，保持统一）。
    pub const fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(0x14, 0x14, 0x18),
            text: Color32::from_rgb(0xE8, 0xE8, 0xEC),
            dim: Color32::from_rgb(0x8A, 0x8A, 0x94),
            hover: Color32::from_rgb(0x2C, 0x2C, 0x34),
            danger: Color32::from_rgb(0xE5, 0x53, 0x4B),
        }
    }


}

enum WBtn {
    Minimize,
    Maximize,
    Close,
}

/// 标题栏 + 边缘 resize 一次接完（非全屏时每帧调用）。
pub fn chrome(parent: &mut Ui, style: TitleBarStyle) {
    ui(parent, style);
    resize_edges(parent);
}

pub fn ui(parent: &mut Ui, style: TitleBarStyle) {
    egui::Panel::top("app-titlebar")
        .exact_size(HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(style.bg).stroke(Stroke::NONE))
        .show(parent, |ui| {
            let rect = ui.max_rect();
            let p = ui.painter();
            let icon_size = 20.0;
            let icon_rect = Rect::from_center_size(
                pos2(rect.left() + 14.0 + icon_size / 2.0, rect.center().y),
                vec2(icon_size, icon_size),
            );
            egui::Image::new("bytes://dyplayer-app-icon.svg")
                .fit_to_exact_size(vec2(icon_size, icon_size))
                .paint_at(ui, icon_rect);

            let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));


            // 拖动区（整条标题栏；按钮稍后 interact，z 更高优先命中）。
            // 必须 click_and_drag：double_clicked 需要 CLICKED flag（仅 click
            // sense 设置），纯 drag() 下双击最大化恒 false（从未生效的旧 bug）。
            let drag = ui.interact(
                rect,
                Id::unique("app-titlebar-drag"),
                Sense::CLICK | Sense::DRAG,
            );
            if drag.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            } else if drag.drag_started_by(egui::PointerButton::Primary) && !maximized {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }

            // 右侧三钮（右→左）：关闭 / 最大化还原 / 最小化。
            let btn_w = 44.0;
            for (i, kind) in [WBtn::Close, WBtn::Maximize, WBtn::Minimize]
                .into_iter()
                .enumerate()
            {
                let r = Rect::from_min_max(
                    pos2(rect.right() - btn_w * (i as f32 + 1.0), rect.top()),
                    pos2(rect.right() - btn_w * i as f32, rect.bottom()),
                );
                let resp = ui.interact(r, Id::unique("app-titlebar-btn").with(i), Sense::CLICK);
                let hovered = resp.hovered();
                let (bg, fg) = match (&kind, hovered) {
                    (WBtn::Close, true) => (Some(style.danger), Color32::WHITE),
                    (_, true) => (Some(style.hover), style.text),
                    (_, false) => (None, style.dim),
                };
                if let Some(bg) = bg {
                    p.rect_filled(r, CornerRadius::ZERO, bg);
                }
                let c = r.center();
                match kind {
                    WBtn::Minimize => {
                        p.line_segment(
                            [pos2(c.x - 5.0, c.y), pos2(c.x + 5.0, c.y)],
                            Stroke::new(1.3, fg),
                        );
                    }
                    WBtn::Maximize => {
                        if maximized {
                            // 还原图标：错位双框。
                            let front =
                                Rect::from_center_size(pos2(c.x - 2.0, c.y - 2.0), vec2(9.0, 9.0));
                            let back =
                                Rect::from_center_size(pos2(c.x + 2.0, c.y + 2.0), vec2(9.0, 9.0));
                            p.rect_stroke(
                                back,
                                CornerRadius::same(1),
                                Stroke::new(1.2, fg),
                                StrokeKind::Inside,
                            );
                            p.rect_filled(
                                Rect::from_min_max(
                                    pos2(front.left(), front.bottom() - 2.0),
                                    pos2(front.right(), front.bottom()),
                                ),
                                CornerRadius::ZERO,
                                style.bg,
                            );
                            p.rect_stroke(
                                front,
                                CornerRadius::same(1),
                                Stroke::new(1.2, fg),
                                StrokeKind::Inside,
                            );
                        } else {
                            p.rect_stroke(
                                Rect::from_center_size(c, vec2(10.0, 10.0)),
                                CornerRadius::same(1),
                                Stroke::new(1.2, fg),
                                StrokeKind::Inside,
                            );
                        }
                    }
                    WBtn::Close => {
                        p.line_segment(
                            [pos2(c.x - 4.5, c.y - 4.5), pos2(c.x + 4.5, c.y + 4.5)],
                            Stroke::new(1.3, fg),
                        );
                        p.line_segment(
                            [pos2(c.x - 4.5, c.y + 4.5), pos2(c.x + 4.5, c.y - 4.5)],
                            Stroke::new(1.3, fg),
                        );
                    }
                }
                if resp.clicked() {
                    let cmd = match kind {
                        WBtn::Minimize => egui::ViewportCommand::Minimized(true),
                        WBtn::Maximize => egui::ViewportCommand::Maximized(!maximized),
                        WBtn::Close => egui::ViewportCommand::Close,
                    };
                    ui.ctx().send_viewport_cmd(cmd);
                }
            }
        });
}

/// 窗口边缘 8 向 resize：透明 hit 区 → 系统级 BeginResize（含光标）。
/// 最大化时窗口不可 resize，直接跳过。
pub fn resize_edges(parent: &mut Ui) {
    if parent.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let r = parent.max_rect();
    let (l, rt, t, b) = (r.left(), r.right(), r.top(), r.bottom());
    let edges: [(Rect, egui::ResizeDirection, egui::CursorIcon); 8] = [
        (
            Rect::from_min_max(pos2(l + CORNER, t), pos2(rt - CORNER, t + EDGE)),
            egui::ResizeDirection::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(pos2(l + CORNER, b - EDGE), pos2(rt - CORNER, b)),
            egui::ResizeDirection::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(pos2(l, t + CORNER), pos2(l + EDGE, b - CORNER)),
            egui::ResizeDirection::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_max(pos2(rt - EDGE, t + CORNER), pos2(rt, b - CORNER)),
            egui::ResizeDirection::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_max(pos2(l, t), pos2(l + CORNER, t + CORNER)),
            egui::ResizeDirection::NorthWest,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_max(pos2(rt - CORNER, t), pos2(rt, t + CORNER)),
            egui::ResizeDirection::NorthEast,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_max(pos2(l, b - CORNER), pos2(l + CORNER, b)),
            egui::ResizeDirection::SouthWest,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_max(pos2(rt - CORNER, b - CORNER), pos2(rt, b)),
            egui::ResizeDirection::SouthEast,
            egui::CursorIcon::ResizeNwSe,
        ),
    ];
    for (i, (rect, dir, cursor)) in edges.into_iter().enumerate() {
        let resp = parent.interact(rect, Id::unique("app-edge").with(i), Sense::DRAG);
        if resp.drag_started_by(egui::PointerButton::Primary) {
            parent
                .ctx()
                .send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
        resp.on_hover_cursor(cursor);
    }
}
