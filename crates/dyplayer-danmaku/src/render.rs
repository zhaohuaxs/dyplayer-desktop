//! glyph atlas + epaint Mesh 自绘渲染（自有层，M2）。
//!
//! TDD 6.5 契约：
//! - 测量与绘制同源：与 layout 的 `DanmakuFont`/`px_scale_for_em` 完全一致，
//!   逐字形 advance + kerning 推进，屏幕位置与碰撞边界零偏差
//! - glyph 按 (codepoint, 量化字号, 描边半径) 缓存于单张 atlas 纹理，
//!   fill 与膨胀 outline 双 mask；纹理仅在内容变化（dirty）时上传
//! - 顶点色与纹理像素均为预乘 sRGBA（egui 0.37 约定：`from_white_alpha`
//!   存储 (a,a,a,a)，shader 中 gamma 空间相乘 + premultiplied blending）
//! - 单条弹幕两遍绘制：先全部 outline 再全部 fill（fill 覆盖描边内芯，
//!   相邻字形描边不侵入填充）；弹幕间按遍历序叠加（后者在上）

use ab_glyph::{Font, FontArc, Glyph, GlyphId, Point, PxScale, ScaleFont};
use epaint::{Color32, ColorImage, ImageData, Mesh, Pos2, Rect, TextureId, Vec2, Vertex};
use rustc_hash::FxHashMap;
use std::collections::VecDeque;

use crate::dfm::{FrameItem, PreparedItem};
use crate::layout::DanmakuFont;
use crate::normalize::{DyGradientTarget, DyStyle};
use crate::outline::{raster_radius, resolve_width_px};
use crate::typography::px_scale_for_em;

const ATLAS_INITIAL_SIZE: u32 = 512;
const ATLAS_MAX_SIZE: u32 = 2048;
const ATLAS_PADDING: u16 = 1;
/// 单字形栅格化像素预算（R06）：异常字号/粗描边可产生数百 MB mask，
/// 在分配前拒绝，避免 UI/覆盖线程失响应或内存耗尽。1M px ≈ 1MB mask。
const GLYPH_MAX_SIDE: usize = 1024;
const GLYPH_MAX_PIXELS: usize = 1 << 20;

/// 弹幕渲染所需的帧参数（UI 侧每帧构造）。
pub struct MeshParams<'a> {
    pub font: &'a DanmakuFont,
    /// 描边 profile（与生成当前 layout 的 LayoutParams.outline_profile 一致）。
    pub outline_profile: f64,
    /// 视频显示区域（逻辑坐标，egui points）。
    pub video_rect: Rect,
    pub pixels_per_point: f32,
    /// 全局不透明度（0.0 - 1.0，用户"弹幕透明度"设置）。
    pub global_opacity: f32,
    pub texture_id: TextureId,
    /// dy 扩展渐变开关（默认关：per-item 渐变降级默认配色，即时项不重排）。
    pub gradient_enabled: bool,
    /// 渐变端点对称收缩量（0-0.25）：t < inset 恒首节点色、t > 1-inset
    /// 恒尾节点色，渐变压缩到中段（渲染参数即时生效）。
    pub gradient_inset: f32,
}

/// 渐变重参数化：文本内 t（0-1）映射到收缩区间 [inset, 1-inset] 上的
/// 归一化参数（CSS linear-gradient 色标语义）。inset 非法（≤0 或 ≥0.5）
/// 时回退恒等映射。inset=0.25、t=0.2 → 0；t=0.5 → 0.5；t=0.8 → 1。
fn remap_gradient_t(t: f32, inset: f32) -> f32 {
    if !inset.is_finite() || inset <= 0.0 || inset >= 0.5 {
        return t.clamp(0.0, 1.0);
    }
    ((t - inset) / (1.0 - 2.0 * inset)).clamp(0.0, 1.0)
}

/// 单个字形在图集内的位置与度量（物理像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AtlasGlyph {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// 相对基线原点的绘制偏移（y 向下）。
    pub offset_x: f32,
    pub offset_y: f32,
    /// 水平步进（与测量同源）。
    pub advance: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct GlyphKey {
    cp: char,
    /// px_per_em 量化到 0.25px（档位字号离散，防浮点漂移产生重复缓存）。
    size_q: u32,
    /// 描边膨胀半径（0 = fill）。
    radius: u16,
}

fn quantize_size(px_per_em: f32) -> u32 {
    if px_per_em.is_finite() && px_per_em > 0.0 {
        (px_per_em * 4.0).round() as u32
    } else {
        0
    }
}

/// glyph 纹理图集：shelf 行打包，写满自动翻倍（清空重排，本帧重新栅格化）。
///
/// 纹理上传由 UI 侧负责：每帧检查 `dirty()`，为真时 `TextureHandle::set(image(), LINEAR)`。
pub struct GlyphAtlas {
    size: u32,
    pixels: Vec<Color32>,
    cursor_x: u16,
    cursor_y: u16,
    shelf_height: u16,
    entries: FxHashMap<GlyphKey, AtlasGlyph>,
    dirty: bool,
    /// 容量耗尽标记：请求调用方在下一帧整体 reset 重排（清掉旧字号
    /// 堆积），防止零尺寸 fallback 被永久缓存导致字形丢失。
    needs_reset: bool,
    /// 连续 reset 次数（R14：reset 后若本帧仍 needs_reset 说明当前字形
    /// 组合本身装不下，不再重复 reset/prime 循环；字形需求变化后归零）。
    consecutive_resets: u32,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

impl GlyphAtlas {
    pub fn new() -> Self {
        let size = ATLAS_INITIAL_SIZE;
        Self {
            size,
            pixels: vec![Color32::TRANSPARENT; (size * size) as usize],
            cursor_x: 0,
            cursor_y: 0,
            shelf_height: 0,
            entries: FxHashMap::default(),
            dirty: true,
            needs_reset: false,
            consecutive_resets: 0,
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// 取走容量耗尽标记（true 表示调用方应整体 reset 重排）。
    #[cfg(test)]
    pub(crate) fn take_needs_reset(&mut self) -> bool {
        std::mem::take(&mut self.needs_reset)
    }

    /// 判断是否应执行 reset（R14）：首次 needs_reset 允许 reset；若上次刚
    /// reset 仍 needs_reset（当前字形组合本身装不下），不再重复 reset。
    /// 本帧无 needs_reset 时归零计数器，恢复后续自愈。
    pub(crate) fn should_reset(&mut self) -> bool {
        if !self.needs_reset {
            self.consecutive_resets = 0;
            return false;
        }
        self.needs_reset = false;
        self.consecutive_resets = self.consecutive_resets.saturating_add(1);
        self.consecutive_resets <= 1
    }

    /// 清空并按给定尺寸重新分配（容量耗尽自愈 / 测试用）。
    pub fn reset(&mut self, size: u32) {
        self.size = size;
        self.pixels = vec![Color32::TRANSPARENT; (size * size) as usize];
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.shelf_height = 0;
        self.entries.clear();
        self.dirty = true;
    }

    /// 容量扩容（不清空重排）：旧像素复制到新缓冲左上角，entries/cursor/
    /// shelf_height 保留（已写字形坐标不变），后续 alloc 继续在新增空间推进。
    ///
    /// 用于 rasterize 内单字形装不下的渐进扩容——避免 reset 清空 entries 导致
    /// 本帧已 prime 的全部字形重栅格化（首帧/换字体时 atlas 翻倍那一帧的
    /// 长帧根因；与 ensure_texture 时序修复同族，治"暂停改设置→播放卡顿"）。
    fn grow(&mut self, new_size: u32) {
        debug_assert!(new_size > self.size, "grow 必须扩大尺寸");
        let old_side = self.size as usize;
        let new_side = new_size as usize;
        let mut new_pixels = vec![Color32::TRANSPARENT; new_side * new_side];
        for row in 0..old_side {
            new_pixels[row * new_side..row * new_side + old_side]
                .copy_from_slice(&self.pixels[row * old_side..(row + 1) * old_side]);
        }
        self.size = new_size;
        self.pixels = new_pixels;
        self.dirty = true;
    }

    /// 图集快照（dirty 时上传）。
    pub fn image(&self) -> ImageData {
        ImageData::Color(std::sync::Arc::new(ColorImage {
            size: [self.size as usize, self.size as usize],
            source_size: Vec2::new(self.size as f32, self.size as f32),
            pixels: self.pixels.clone(),
        }))
    }

    /// 取字形（缓存未命中则栅格化并写入图集）。
    ///
    /// 返回 None 仅在字形超出最大图集尺寸（防御路径，正常不发生）；
    /// 无轮廓字形（空格等）返回零尺寸 entry（advance 仍有效）。
    pub(crate) fn glyph(
        &mut self,
        font: &FontArc,
        ch: char,
        px_per_em: f32,
        radius: u16,
    ) -> Option<AtlasGlyph> {
        let key = GlyphKey {
            cp: ch,
            size_q: quantize_size(px_per_em),
            radius,
        };
        if let Some(entry) = self.entries.get(&key) {
            return Some(*entry);
        }
        let scale = px_scale_for_em(font, px_per_em);
        let scaled = font.as_scaled(scale);
        let id = font.glyph_id(ch);
        let advance = scaled.h_advance(id);

        let entry = match self.rasterize(font, id, scale, radius, advance) {
            Some(entry) => entry,
            None => AtlasGlyph {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
                offset_x: 0.0,
                offset_y: 0.0,
                advance,
            },
        };
        self.entries.insert(key, entry);
        Some(entry)
    }

    fn rasterize(
        &mut self,
        font: &FontArc,
        id: GlyphId,
        scale: PxScale,
        radius: u16,
        advance: f32,
    ) -> Option<AtlasGlyph> {
        let glyph = Glyph {
            id,
            scale,
            position: Point { x: 0.0, y: 0.0 },
        };
        let outlined = font.outline_glyph(glyph)?;
        let bounds = outlined.px_bounds();
        let w = bounds.width().ceil().max(0.0) as usize;
        let h = bounds.height().ceil().max(0.0) as usize;
        if w == 0 || h == 0 || w > u16::MAX as usize || h > u16::MAX as usize {
            return None;
        }
        // 栅格化内存预算（R06）：膨胀后尺寸 (w+2r)*(h+2r) 先于分配检查，
        // 超限直接拒绝（调用方回零尺寸 fallback），不进入 w*h mask 分配。
        let dw = w.saturating_add(2 * radius as usize);
        let dh = h.saturating_add(2 * radius as usize);
        if dw > GLYPH_MAX_SIDE || dh > GLYPH_MAX_SIDE || dw.saturating_mul(dh) > GLYPH_MAX_PIXELS {
            return None;
        }
        let mut mask = vec![0u8; w * h];
        outlined.draw(|x, y, v| {
            if let Some(slot) = mask.get_mut(y as usize * w + x as usize) {
                *slot = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });

        let (mask, w, h, offset_x, offset_y) = if radius > 0 {
            let (mask, w, h) = dilate_mask(&mask, w, h, radius);
            let r = f32::from(radius);
            (mask, w, h, bounds.min.x - r, bounds.min.y - r)
        } else {
            (mask, w, h, bounds.min.x, bounds.min.y)
        };

        let (px, py) = match self.alloc(w as u16, h as u16) {
            Some(pos) => pos,
            None => {
                if self.size >= ATLAS_MAX_SIZE {
                    // 容量已到上限：标记下一帧整体重排。本字形返回
                    // 零尺寸 fallback（会被 reset 连同缓存清除）。
                    self.needs_reset = true;
                    return None;
                }
                // 渐进扩容（不清空）：保留已 prime 字形，避免本帧重栅格化长帧。
                self.grow(self.size * 2);
                self.alloc(w as u16, h as u16)?
            }
        };
        let side = self.size as usize;
        for row in 0..h {
            for col in 0..w {
                let a = mask[row * w + col];
                let dst = (py as usize + row) * side + px as usize + col;
                self.pixels[dst] = Color32::from_white_alpha(a);
            }
        }
        self.dirty = true;
        Some(AtlasGlyph {
            x: px,
            y: py,
            w: w as u16,
            h: h as u16,
            offset_x,
            offset_y,
            advance,
        })
    }

    /// shelf 行分配；返回左上像素坐标。
    fn alloc(&mut self, w: u16, h: u16) -> Option<(u16, u16)> {
        let side = self.size as u16;
        let w = w.saturating_add(ATLAS_PADDING);
        let h = h.saturating_add(ATLAS_PADDING);
        if w > side || h > side {
            return None;
        }
        if self.cursor_x + w > side {
            self.cursor_y = self.cursor_y.saturating_add(self.shelf_height);
            self.cursor_x = 0;
            self.shelf_height = 0;
        }
        if self.cursor_y + h > side {
            return None;
        }
        let pos = (self.cursor_x, self.cursor_y);
        self.cursor_x += w;
        self.shelf_height = self.shelf_height.max(h);
        Some(pos)
    }

}

/// 方形核形态学膨胀（描边 mask 生成，vendor outline.rs 的 alpha-mask dilator 契约）。
///
/// 用**可分离**优化：方形核 (2r+1)² = 水平 1D ⊕ 垂直 1D，两趟滑动窗口 max
/// （monotone deque），复杂度 O(w·h·(2r+1))，vs 圆形核暴力 O(w·h·(2r+1)²)。
/// 方形 vs 圆形：典型 r=3-5 时外缘角落差 <1px，且描边外缘由 fill 内芯覆盖，
/// 视觉差异可忽略；大 r（极端 clamp 16）描边略方，可接受。
fn dilate_mask(mask: &[u8], w: usize, h: usize, radius: u16) -> (Vec<u8>, usize, usize) {
    let r = radius as usize;
    if r == 0 {
        return (mask.to_vec(), w, h);
    }
    let ow = w + 2 * r;
    let oh = h + 2 * r;
    // 水平趟：src w×h → tmp ow×h（每行窗口半宽 r，输出左右各扩 r padding）。
    let mut tmp = vec![0u8; ow * h];
    for y in 0..h {
        dilate_row_1d(&mask[y * w..y * w + w], r, &mut tmp[y * ow..y * ow + ow]);
    }
    // 垂直趟：tmp ow×h → out ow×oh（每列窗口半宽 r，输出上下各扩 r padding）。
    let mut out = vec![0u8; ow * oh];
    let mut col = vec![0u8; h];
    let mut col_out = vec![0u8; oh];
    for x in 0..ow {
        for y in 0..h {
            col[y] = tmp[y * ow + x];
        }
        dilate_row_1d(&col, r, &mut col_out);
        for y in 0..oh {
            out[y * ow + x] = col_out[y];
        }
    }
    (out, ow, oh)
}

/// 1D 膨胀（滑动窗口 max，monotone deque）。
/// `src` 长 n，`dst` 长 n+2r：`dst[di] = max{ src[si] : si ∈ [di−2r, di] ∩ [0,n) }`，
/// 越界视为 0。每个 di 均摊 O(1)，整行 O(n+2r)。
fn dilate_row_1d(src: &[u8], r: usize, dst: &mut [u8]) {
    let n = src.len();
    let mut dq: VecDeque<usize> = VecDeque::new();
    for di in 0..dst.len() {
        // 新源索引 di 进入窗口右端（若在 src 范围内）。
        if di < n {
            while let Some(&back) = dq.back() {
                if src[back] <= src[di] {
                    dq.pop_back();
                } else {
                    break;
                }
            }
            dq.push_back(di);
        }
        // 移除窗口左端之外的过期索引。
        let lo = di.saturating_sub(2 * r);
        while let Some(&front) = dq.front() {
            if front < lo {
                dq.pop_front();
            } else {
                break;
            }
        }
        dst[di] = dq.front().map_or(0, |&f| src[f]);
    }
}

fn resolve_outline_radius(px_per_em: f32, outline_profile: f64) -> u16 {
    if !outline_profile.is_finite() || outline_profile <= 0.0 {
        return 0;
    }
    raster_radius(resolve_width_px(px_per_em, outline_profile as f32))
}

fn argb_to_rgb(argb: i32) -> (u8, u8, u8) {
    let v = argb as u32;
    (
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    )
}

/// 生成当前帧弹幕 Mesh（绘制于视频之上、UI 之下）。
///
/// 两阶段：先预取所有字形（此阶段可能触发图集扩容重排，不产生输出），
/// 再生成顶点（全部命中缓存，UV 稳定）。
pub fn build_danmaku_mesh(
    atlas: &mut GlyphAtlas,
    params: &MeshParams<'_>,
    items: &[(FrameItem, &PreparedItem)],
) -> Mesh {
    let font = params.font.font();
    // 容量耗尽自愈：整体重排后本帧 prime 全部重新装入（只保留当前帧
    // 需要的字形，清掉旧字号档位堆积）。
    if atlas.should_reset() {
        atlas.reset(atlas.size());
    }
    for (frame, item) in items {
        let px_per_em = item.font_size as f32;
        let radius = resolve_outline_radius(px_per_em, params.outline_profile);
        prime_atlas(atlas, font, item, radius);
        prime_atlas(atlas, font, item, 0);
        let _ = frame;
    }

    let atlas_side = atlas.size() as f32;
    let inv_ppp = 1.0 / params.pixels_per_point.max(0.001);
    let origin = params.video_rect.left_top();

    let mut mesh = Mesh::with_texture(params.texture_id);
    for (frame, item) in items {
        let px_per_em = item.font_size as f32;
        let radius = resolve_outline_radius(px_per_em, params.outline_profile);
        let frame_alpha = item.opacity.clamp(0.0, 1.0) * frame.alpha.clamp(0.0, 1.0);
        let alpha = (frame_alpha * params.global_opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        if alpha == 0 {
            continue;
        }
        let (r, g, b) = argb_to_rgb(item.color_argb);
        // dy 扩展渐变（开关关 = None，回归默认配色）：
        // target=Outline → 描边渐变、文本强制白；target=Fill → 文本渐变、
        // 描边固定黑；无样式 → 描边黑、文本原色（现状）。
        let style = if params.gradient_enabled {
            item.style.as_ref()
        } else {
            None
        };
        let (outline_gradient, outline_base) = match style
            .filter(|s| s.target == DyGradientTarget::Outline)
        {
            Some(s) => (Some(s), (0u8, 0u8, 0u8)),
            None => {
                let l = (r as u32 * 11 + g as u32 * 16 + b as u32 * 5) / 32;
                (
                    None,
                    if l < 30 {
                        (255u8, 255u8, 255u8)
                    } else {
                        (0u8, 0u8, 0u8)
                    },
                )
            }
        };
        let (fill_gradient, fill_base) = match style {
            Some(s) if s.target == DyGradientTarget::Fill => (Some(s), (r, g, b)),
            Some(_) => (None, (255u8, 255u8, 255u8)),
            None => (None, (r, g, b)),
        };
        let v_start = mesh.vertices.len();
        if radius > 0 {
            push_text_pass(
                &mut mesh,
                atlas,
                font,
                frame,
                item,
                radius,
                outline_base,
                outline_gradient,
                params.gradient_inset,
                alpha,
                origin,
                inv_ppp,
                atlas_side,
            );
        }
        push_text_pass(
            &mut mesh,
            atlas,
            font,
            frame,
            item,
            0,
            fill_base,
            fill_gradient,
            params.gradient_inset,
            alpha,
            origin,
            inv_ppp,
            atlas_side,
        );
        // mode7 zRotate 2D 旋转（绕文本中心；实测全 0，兜底）。
        let z_rotate = item.special.as_ref().map(|s| s.z_rotate).unwrap_or(0.0);
        if z_rotate != 0.0 {
            let cx = origin.x + (frame.x as f32 + item.width as f32 * 0.5) * inv_ppp;
            let cy = origin.y + (frame.y as f32 + item.height as f32 * 0.5) * inv_ppp;
            let cos = z_rotate.cos();
            let sin = z_rotate.sin();
            for v in &mut mesh.vertices[v_start..] {
                let dx = v.pos.x - cx;
                let dy = v.pos.y - cy;
                v.pos.x = cx + dx * cos - dy * sin;
                v.pos.y = cy + dx * sin + dy * cos;
            }
        }
    }
    mesh
}

fn prime_atlas(atlas: &mut GlyphAtlas, font: &FontArc, item: &PreparedItem, radius: u16) {
    let px_per_em = item.font_size as f32;
    for ch in item.text.chars() {
        if ch.is_control() {
            continue;
        }
        atlas.glyph(font, ch, px_per_em, radius);
    }
}

/// 单条弹幕单遍字形推入（radius=0 为 fill 遍，radius>0 为 outline 遍）。
///
/// pen 推进与 `DanmakuFont::measure` 完全同构（h_advance + kerning）。
/// 无渐变时全部 quad 用 base_rgb + alpha；带渐变时每 quad 按其 x 范围
/// 占弹幕总宽比例采样节点色（顶点色插值，gamma 空间，与 epaint 渐变
/// 同路径；颜色不进 atlas 纹理，零缓存影响）。
#[allow(clippy::too_many_arguments)]
fn push_text_pass(
    mesh: &mut Mesh,
    atlas: &mut GlyphAtlas,
    font: &FontArc,
    frame: &FrameItem,
    item: &PreparedItem,
    radius: u16,
    base_rgb: (u8, u8, u8),
    gradient: Option<&DyStyle>,
    gradient_inset: f32,
    alpha: u8,
    origin: Pos2,
    inv_ppp: f32,
    atlas_side: f32,
) {
    let px_per_em = item.font_size as f32;
    let scale = px_scale_for_em(font, px_per_em);
    let scaled = font.as_scaled(scale);
    let line_height = scaled.ascent() - scaled.descent();
    let mut baseline = frame.y as f32 + scaled.ascent();
    // pen_x = 屏幕绝对 x（滚动位置，只管 quad 位置）；text_x = 文本内相对
    // x（从 0 起，只管渐变参数）。渐变 t 必须用文本内坐标：拿 pen_x 除
    // 文本宽再 clamp 会在 frame.x 远大于文本宽时（滚动入屏右侧/固定弹幕
    // 居中）恒被夹到 t=1，整条弹幕退化单色；只有弹幕恰好滚到屏幕左端时
    // 渐变才可见（回归：非零屏幕位置的渐变顶点色测试）。
    let mut pen_x = frame.x as f32;
    let mut text_x = 0.0f32;
    let total_w = (item.width as f32).max(1.0);
    let (br, bg, bb) = base_rgb;
    let base_color = Color32::from_rgba_unmultiplied(br, bg, bb, alpha);
    let mut prev: Option<GlyphId> = None;
    for ch in item.text.chars() {
        if ch == '\n' {
            pen_x = frame.x as f32;
            text_x = 0.0;
            baseline += line_height;
            prev = None;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        let id = font.glyph_id(ch);
        if let Some(prev_id) = prev {
            let kern = scaled.kern(prev_id, id);
            pen_x += kern;
            text_x += kern;
        }
        if let Some(entry) = atlas.glyph(font, ch, px_per_em, radius)
            && entry.w > 0
            && entry.h > 0
        {
            let (c_left, c_right) = match gradient {
                Some(style) => {
                    let t0 = ((text_x + entry.offset_x) / total_w).clamp(0.0, 1.0);
                    let t1 =
                        ((text_x + entry.offset_x + f32::from(entry.w)) / total_w).clamp(0.0, 1.0);
                    let (r0, g0, b0) = sample_gradient(&style.nodes, remap_gradient_t(t0, gradient_inset));
                    let (r1, g1, b1) = sample_gradient(&style.nodes, remap_gradient_t(t1, gradient_inset));
                    (
                        Color32::from_rgba_unmultiplied(r0, g0, b0, alpha),
                        Color32::from_rgba_unmultiplied(r1, g1, b1, alpha),
                    )
                }
                None => (base_color, base_color),
            };
            push_glyph_quad(
                mesh,
                atlas_side,
                entry,
                pen_x,
                baseline,
                origin,
                inv_ppp,
                c_left,
                c_right,
            );
        }
        let adv = scaled.h_advance(id);
        pen_x += adv;
        text_x += adv;
        prev = Some(id);
    }
}

/// 节点渐变采样：节点均匀分段（2 节点单段、3 节点两段），段内 gamma
/// 空间线性插值（RGB 直插，与 epaint 顶点色渐变一致）。
/// 节点数不足 2 时回白色（防御：解析层已保证 >=2）。
fn sample_gradient(nodes: &[u32], t: f32) -> (u8, u8, u8) {
    if nodes.len() < 2 {
        return (255, 255, 255);
    }
    let t = t.clamp(0.0, 1.0);
    let seg = 1.0 / (nodes.len() - 1) as f32;
    let idx = ((t / seg) as usize).min(nodes.len() - 2);
    let local = (t - idx as f32 * seg) / seg;
    let lerp = |a: u32, b: u32| (a as f32 + (b as f32 - a as f32) * local).round().clamp(0.0, 255.0) as u8;
    let a = nodes[idx];
    let b = nodes[idx + 1];
    (
        lerp((a >> 16) & 0xFF, (b >> 16) & 0xFF),
        lerp((a >> 8) & 0xFF, (b >> 8) & 0xFF),
        lerp(a & 0xFF, b & 0xFF),
    )
}

/// 字形 quad 推入：mesh 顶点直接以物理像素坐标写入（调用侧已乘 inv_ppp）。
/// 水平渐变：左列顶点 c_left、右列 c_right（无渐变时两者相等）。
#[allow(clippy::too_many_arguments)]
fn push_glyph_quad(
    mesh: &mut Mesh,
    atlas_side: f32,
    entry: AtlasGlyph,
    pen_x: f32,
    baseline: f32,
    origin: Pos2,
    inv_ppp: f32,
    c_left: Color32,
    c_right: Color32,
) {
    let min = origin
        + Vec2::new(
            (pen_x + entry.offset_x) * inv_ppp,
            (baseline + entry.offset_y) * inv_ppp,
        );
    let max = min + Vec2::new(f32::from(entry.w) * inv_ppp, f32::from(entry.h) * inv_ppp);
    // UV 半 texel 内缩：LINEAR 采样下防止相邻字形渗色
    let u0 = (f32::from(entry.x) + 0.5) / atlas_side;
    let u1 = (f32::from(entry.x + entry.w) - 0.5) / atlas_side;
    let v0 = (f32::from(entry.y) + 0.5) / atlas_side;
    let v1 = (f32::from(entry.y + entry.h) - 0.5) / atlas_side;

    let idx = mesh.vertices.len() as u32;
    mesh.indices
        .extend_from_slice(&[idx, idx + 1, idx + 2, idx + 2, idx + 1, idx + 3]);
    mesh.vertices.extend_from_slice(&[
        Vertex {
            pos: Pos2::new(min.x, min.y),
            uv: Pos2::new(u0, v0),
            color: c_left,
        },
        Vertex {
            pos: Pos2::new(max.x, min.y),
            uv: Pos2::new(u1, v0),
            color: c_right,
        },
        Vertex {
            pos: Pos2::new(min.x, max.y),
            uv: Pos2::new(u0, v1),
            color: c_left,
        },
        Vertex {
            pos: Pos2::new(max.x, max.y),
            uv: Pos2::new(u1, v1),
            color: c_right,
        },
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys_font() -> Option<DanmakuFont> {
        DanmakuFont::from_system()
    }

    fn test_item(text: &str, color_argb: i32, font_size: f64) -> PreparedItem {
        PreparedItem {
            source_id: 0,
            time_seconds: 0.0,
            text: text.to_string(),
            type_code: 1,
            color_argb,
            opacity: 1.0,
            is_me: false,
            font_size,
            font_size_multiplier: 1.0,
            count_text: None,
            duplicate_count: 1,
            track_index: 0,
            y_position: 0.0,
            width: 100.0,
            height: 30.0,
            scroll_speed: 100.0,
            is_filtered: false,
            duration_seconds: 10.0,
            is_scroll: true,
            centered_x: 0.0,
            style: None,
            special: None,
        }
    }

    fn test_frame(x: f64, y: f64) -> FrameItem {
        FrameItem {
            item_index: 0,
            track_index: 0,
            x,
            y,
            offstage_x: 0.0,
            alpha: 1.0,
        }
    }

    #[test]
    fn dilate_square_kernel_expands_to_corners() {
        let mask = vec![255u8; 9];
        let (out, w, h) = dilate_mask(&mask, 3, 3, 1);
        assert_eq!((w, h), (5, 5));
        let at = |r: usize, c: usize| r * 5 + c;
        assert_eq!(out[at(2, 2)], 255, "中心保持");
        assert_eq!(out[at(2, 1)], 255, "边邻在半径 1 内");
        assert_eq!(out[at(0, 2)], 255, "顶中距 fill 边 1");
        // 方形核：角 (0,0) 的窗口覆盖源 (0,0)（距 1 ≤ r=1），膨胀到 255。
        // 与圆形核（角为 0）不同，方形可分离的代价是角落略方。
        assert_eq!(out[at(0, 0)], 255, "方形核角在 r=1 窗口内");
        assert_eq!(out[at(4, 4)], 255, "方形核对角在 r=1 窗口内");
        let (same, w, h) = dilate_mask(&mask, 3, 3, 0);
        assert_eq!((w, h), (3, 3));
        assert_eq!(same, mask);
    }

    #[test]
    fn dilate_separable_matches_naive_square() {
        // 方形核可分离结果 == 朴素方形核暴力实现，全随机输入多半径验证。
        fn naive_square(mask: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
            let ow = w + 2 * r;
            let oh = h + 2 * r;
            let mut out = vec![0u8; ow * oh];
            for oy in 0..oh {
                let cy = oy as isize - r as isize;
                for ox in 0..ow {
                    let cx = ox as isize - r as isize;
                    let mut best = 0u8;
                    for yy in (cy - r as isize).max(0)..=(cy + r as isize).min(h as isize - 1) {
                        for xx in (cx - r as isize).max(0)..=(cx + r as isize).min(w as isize - 1) {
                            best = best.max(mask[yy as usize * w + xx as usize]);
                        }
                    }
                    out[oy * ow + ox] = best;
                }
            }
            out
        }
        let mask: Vec<u8> = (0..70).map(|i| (i * 37 % 256) as u8).collect();
        for &r in &[1usize, 2, 3, 5] {
            let (out, w, h) = dilate_mask(&mask, 7, 10, r as u16);
            assert_eq!((w, h), (7 + 2 * r, 10 + 2 * r));
            assert_eq!(out, naive_square(&mask, 7, 10, r), "r={r} 可分离应与朴素方形核一致");
        }
    }

    #[test]
    fn atlas_caches_glyphs_by_size_and_radius() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let font_arc = font.font().clone();
        let mut atlas = GlyphAtlas::new();
        let a1 = atlas.glyph(&font_arc, '弹', 30.0, 0).unwrap();
        let a2 = atlas.glyph(&font_arc, '弹', 30.0, 0).unwrap();
        assert_eq!(a1, a2, "同 key 必须命中缓存");
        assert!(a1.w > 0 && a1.h > 0);
        assert!(a1.advance > 0.0);
        let outline = atlas.glyph(&font_arc, '弹', 30.0, 1).unwrap();
        assert_ne!(outline, a1, "outline 是独立 entry");
        assert!(outline.w > a1.w, "膨胀后必须更宽");
        assert!(atlas.dirty());
        let image = atlas.image();
        assert_eq!((image.width(), image.height()), (512, 512));
        atlas.clear_dirty();
        atlas.glyph(&font_arc, '弹', 30.0, 0);
        assert!(!atlas.dirty(), "缓存命中不得置脏");
    }

    #[test]
    fn atlas_grows_when_exhausted() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let font_arc = font.font().clone();
        let mut atlas = GlyphAtlas::new();
        let chars: Vec<char> =
            "零一二三四五六七八九十百千万亿甲乙丙丁戊己庚辛壬癸子丑寅卯辰巳午未申酉戌亥"
                .chars()
                .collect();
        for &ch in &chars {
            atlas.glyph(&font_arc, ch, 120.0, 0);
        }
        assert!(atlas.size() > 512, "24 个 120px 汉字应触发扩容");
        for &ch in &chars {
            assert!(
                atlas.glyph(&font_arc, ch, 120.0, 0).is_some(),
                "扩容后全部可命中"
            );
        }
    }

    /// 复现：多档字号 glyph 堆积耗尽 MAX atlas 后字形永久丢失（渲染残缺）。
    #[test]
    fn atlas_overflow_requests_reset_and_recovers() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let font_arc = font.font().clone();
        let mut atlas = GlyphAtlas::new();
        // 连续 CJK unique 字 × 多档字号（模拟滑块拖动产生的档位），
        // 强制耗尽 2048 上限。
        let mut lost = 0usize;
        'outer: for step in 0..7usize {
            let size = 12.0 + step as f32 * 6.0;
            for cp in 0x4E00u32..0x9FA6 {
                let Some(ch) = char::from_u32(cp) else { continue };
                let entry = atlas.glyph(&font_arc, ch, size, 3).unwrap();
                if entry.w == 0 {
                    lost += 1;
                    if lost >= 10 {
                        break 'outer;
                    }
                }
            }
        }
        assert!(lost >= 10, "应已耗尽容量出现零尺寸 fallback，实际 lost={lost}");
        assert!(atlas.size() == ATLAS_MAX_SIZE);
        // 溢出必须请求整体重排，且重排后能重新装入（自愈，不永久丢失）。
        assert!(atlas.take_needs_reset(), "耗尽后必须请求 reset");
        atlas.reset(atlas.size());
        let entry = atlas.glyph(&font_arc, '永', 48.0, 3).unwrap();
        assert!(entry.w > 0, "reset 后字形必须恢复");
    }

    /// grow 不清空重排：旧字形 entry 坐标不变、像素复制到新缓冲对应位置，
    /// 后续 alloc 继续在新增空间推进。这是治"atlas 翻倍那一帧全部字形重
    /// 栅格化长帧"的关键不变量（与 ensure_texture 时序修复同族）。
    #[test]
    fn atlas_grow_preserves_existing_glyphs_and_pixels() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let font_arc = font.font().clone();
        let mut atlas = GlyphAtlas::new();
        let old_size = atlas.size();
        let e1 = atlas.glyph(&font_arc, '字', 48.0, 0).unwrap();
        assert!(e1.w > 0 && e1.h > 0, "字形必须有效");
        // 旧像素区域快照（含 padding 行）。
        let side_old = old_size as usize;
        let old_pixels: Vec<Color32> = (0..e1.h as usize)
            .flat_map(|row| {
                let start = (e1.y as usize + row) * side_old + e1.x as usize;
                atlas.pixels[start..start + e1.w as usize].to_vec()
            })
            .collect();
        // grow 后旧字形应直接命中缓存（不重新栅格化），坐标完全不变。
        atlas.grow(old_size * 2);
        assert_eq!(atlas.size(), old_size * 2);
        let e2 = atlas.glyph(&font_arc, '字', 48.0, 0).unwrap();
        assert_eq!((e2.x, e2.y, e2.w, e2.h), (e1.x, e1.y, e1.w, e1.h));
        // 旧像素已复制到新缓冲对应位置（坐标不变，行 stride 变大）。
        let new_side = atlas.size() as usize;
        let new_pixels: Vec<Color32> = (0..e1.h as usize)
            .flat_map(|row| {
                let start = (e1.y as usize + row) * new_side + e1.x as usize;
                atlas.pixels[start..start + e1.w as usize].to_vec()
            })
            .collect();
        assert_eq!(new_pixels, old_pixels, "grow 后旧字形像素必须原位保留");
        // 新字形可在新增空间 alloc 写入（不同字符、同档位）。
        let e3 = atlas.glyph(&font_arc, '画', 48.0, 0).unwrap();
        assert!(e3.w > 0 && e3.h > 0, "新字形必须可写入扩容后的空间");
        assert_ne!((e3.x, e3.y), (e1.x, e1.y), "新字形不能覆盖旧字形位置");
    }

    #[test]
    fn mesh_emits_outline_then_fill_with_correct_colors() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let item = test_item("abc", 0xFFFF_0000u32 as i32, 24.0);
        let frame = test_frame(10.0, 20.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 2.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(7),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        assert_eq!(mesh.texture_id, TextureId::Managed(7));
        assert_eq!(mesh.indices.len() % 3, 0);
        assert_eq!(mesh.indices.len() / 6, 6, "3 字符 × (outline + fill)");
        let outline_color = Color32::from_rgba_unmultiplied(0, 0, 0, 255);
        for v in &mesh.vertices[0..12] {
            assert_eq!(v.color, outline_color, "前 3 quad 是黑色描边");
        }
        let fill_color = Color32::from_rgba_unmultiplied(255, 0, 0, 255);
        for v in &mesh.vertices[12..24] {
            assert_eq!(v.color, fill_color, "后 3 quad 是红色填充");
        }
        for v in &mesh.vertices {
            assert!((0.0..=1.0).contains(&v.uv.x));
            assert!((0.0..=1.0).contains(&v.uv.y));
            assert!(v.pos.x >= 0.0 && v.pos.y >= 0.0);
        }
    }

    #[test]
    fn outline_color_adapts_to_text_brightness() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let params = MeshParams {
            font: &font,
            outline_profile: 2.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        // qGray = (r*11 + g*16 + b*5) / 32；阈值 < 30 → 白描边，否则黑描边。
        for (color_argb, expect_white_outline, label) in [
            (0xFF00_0000u32 as i32, true, "纯黑 → 白描边"),
            (0xFFFF_FFFFu32 as i32, false, "纯白 → 黑描边"),
            (0xFF00_00BFu32 as i32, true, "亮度 29 → 白描边"),
            (0xFF00_00C0u32 as i32, false, "亮度 30 → 黑描边"),
        ] {
            let mut atlas = GlyphAtlas::new();
            let item = test_item("a", color_argb, 24.0);
            let frame = test_frame(10.0, 20.0);
            let items = vec![(frame, &item)];
            let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
            // 单字符：前 4 顶点 = outline quad，后 4 顶点 = fill quad。
            let expected = if expect_white_outline {
                Color32::from_rgba_unmultiplied(255, 255, 255, 255)
            } else {
                Color32::from_rgba_unmultiplied(0, 0, 0, 255)
            };
            for v in &mesh.vertices[0..4] {
                assert_eq!(v.color, expected, "{label}: 描边顶点色");
            }
        }
    }

    #[test]
    fn sample_gradient_two_and_three_nodes() {
        // 两节点：端点原色、中点各半（15880350 = 0xF2509E、3181517 = 0x308BCD）。
        let nodes = [15880350u32, 3181517];
        assert_eq!(sample_gradient(&nodes, 0.0), (242, 80, 158));
        assert_eq!(sample_gradient(&nodes, 1.0), (48, 139, 205));
        let mid = sample_gradient(&nodes, 0.5);
        assert_eq!(mid, (145, 110, 182));
        // t 超界 clamp。
        assert_eq!(sample_gradient(&nodes, -1.0), (242, 80, 158));
        assert_eq!(sample_gradient(&nodes, 2.0), (48, 139, 205));
        // 三节点：0.25 = 第一段中点（A、B 各半），0.75 = 第二段中点。
        let nodes3 = [0xFF0000u32, 0x00FF00, 0x0000FF];
        let q1 = sample_gradient(&nodes3, 0.25);
        assert_eq!(q1, (128, 128, 0));
        let q3 = sample_gradient(&nodes3, 0.75);
        assert_eq!(q3, (0, 128, 128));
        // 节点不足 2（防御路径）：回白色。
        assert_eq!(sample_gradient(&[0xFF0000], 0.5), (255, 255, 255));
    }

    #[test]
    fn mesh_gradient_fill_vertices_interpolate() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        // 文本渐变（target=Fill）：红→蓝，开关开。width 取实测值（真实
        // 链路 PreparedItem.width = 测量宽 + 描边外扩；profile=0 即测量宽），
        // 字形串恰铺满渐变区间。
        let mut item = test_item("abc", 0xFFFF_FFFFu32 as i32, 24.0);
        item.width = font.measure("abc", 24.0).0;
        item.style = Some(DyStyle {
            nodes: vec![0xFF0000, 0x0000FF],
            target: DyGradientTarget::Fill,
        });
        let frame = test_frame(0.0, 0.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: true,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        assert_eq!(mesh.indices.len() / 6, 3, "无描边 profile 时仅 fill");
        // 首 quad（'a'）：t∈[0, ~1/3] → 近红。
        for v in &mesh.vertices[0..4] {
            let (r, g, b) = (v.color.r(), v.color.g(), v.color.b());
            assert!(r > 170 && g < 20, "首字符应近红，got r={r} g={g} b={b}");
            let _ = b;
        }
        // 末 quad（'c'）：t∈[~2/3, 1] → 近蓝。
        for v in &mesh.vertices[8..12] {
            let (r, g, b) = (v.color.r(), v.color.g(), v.color.b());
            assert!(b > 170 && g < 20, "尾字符应近蓝，got r={r} g={g} b={b}");
            let _ = r;
        }
        // 中间 quad（'b'）：t≈0.5 → 红蓝各半的过渡色。
        for v in &mesh.vertices[4..8] {
            let (r, _, b) = (v.color.r(), v.color.g(), v.color.b());
            assert!(
                (60..=200).contains(&r) && (60..=200).contains(&b),
                "中间字符应有过渡色，got r={r} b={b}"
            );
        }
    }

    #[test]
    fn mesh_gradient_outline_target_forces_white_fill() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        // 描边渐变（target=Outline）：文本强制白。width 对齐真实语义
        // （测量宽 + 描边外扩 ×2）。
        let outline_px = resolve_width_px(24.0, 2.0);
        let mut item = test_item("ab", 0xFF00_0000u32 as i32, 24.0);
        item.width = font.measure("ab", 24.0).0 + f64::from(outline_px) * 2.0;
        item.style = Some(DyStyle {
            nodes: vec![0xFF0000, 0x0000FF],
            target: DyGradientTarget::Outline,
        });
        let frame = test_frame(0.0, 0.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 2.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: true,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        assert_eq!(mesh.indices.len() / 6, 4, "2 字符 × (outline + fill)");
        // 'a' outline quad 左列（t≈0）：近红。
        let r0 = mesh.vertices[0].color.r();
        assert!(r0 > 170, "描边渐变首顶点应近红，got {r0}");
        // 'b' outline quad 右列（t≈1）：近蓝。
        let b_last = mesh.vertices[7].color.b();
        assert!(b_last > 170, "描边渐变尾顶点应近蓝，got {b_last}");
        // 后 2 quad = fill：全部纯白（覆盖原红色）。
        for v in &mesh.vertices[8..16] {
            assert_eq!(
                (v.color.r(), v.color.g(), v.color.b()),
                (255, 255, 255),
                "描边渐变时文本必须白"
            );
        }
    }

    #[test]
    fn remap_gradient_t_inset_semantics() {
        // inset=0：恒等（现状零变化）。
        assert_eq!(remap_gradient_t(0.0, 0.0), 0.0);
        assert_eq!(remap_gradient_t(0.5, 0.0), 0.5);
        assert_eq!(remap_gradient_t(1.0, 0.0), 1.0);
        // inset=0.25：[0.25, 0.75] → [0, 1]，两端夹到纯色。
        assert_eq!(remap_gradient_t(0.2, 0.25), 0.0);
        assert_eq!(remap_gradient_t(0.25, 0.25), 0.0);
        assert_eq!(remap_gradient_t(0.5, 0.25), 0.5);
        assert_eq!(remap_gradient_t(0.75, 0.25), 1.0);
        assert_eq!(remap_gradient_t(0.8, 0.25), 1.0);
        // t 超界先夹入 [0,1] 再重参数化。
        assert_eq!(remap_gradient_t(-1.0, 0.25), 0.0);
        assert_eq!(remap_gradient_t(2.0, 0.25), 1.0);
        // 非法 inset（≤0 / ≥0.5 / NaN）：回退恒等 + clamp。
        assert_eq!(remap_gradient_t(0.5, -0.1), 0.5);
        assert_eq!(remap_gradient_t(0.5, 0.5), 0.5);
        assert_eq!(remap_gradient_t(0.5, f32::NAN), 0.5);
    }

    #[test]
    fn mesh_gradient_inset_shrinks_gradient_band() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let make_mesh = |inset: f32| {
            let mut atlas = GlyphAtlas::new();
            let mut item = test_item("abc", 0xFFFF_FFFFu32 as i32, 24.0);
            item.width = font.measure("abc", 24.0).0;
            item.style = Some(DyStyle {
                nodes: vec![0xFF0000, 0x0000FF],
                target: DyGradientTarget::Fill,
            });
            let frame = test_frame(0.0, 0.0);
            let items = vec![(frame, &item)];
            let params = MeshParams {
                font: &font,
                outline_profile: 0.0,
                video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
                pixels_per_point: 1.0,
                global_opacity: 1.0,
                texture_id: TextureId::Managed(0),
                gradient_enabled: true,
                gradient_inset: inset,
            };
            let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
            mesh.vertices.iter().map(|v| v.color).collect::<Vec<_>>()
        };
        let plain = make_mesh(0.0);
        let inset = make_mesh(0.25);
        assert_eq!(plain.len(), inset.len());
        // inset=0.25 顶点色必须与 inset=0 不同（渐变带被压缩）。
        let differs = plain.iter().zip(inset.iter()).any(|(a, b)| a != b);
        assert!(differs, "inset=0.25 顶点色必须与 inset=0 不同");
        // 左端纯色段证据：'a' 左列 t≈0 < inset → t_norm=0 → 纯首色（红）。
        assert_eq!(
            (inset[0].r(), inset[0].g(), inset[0].b()),
            (255, 0, 0),
            "t < inset 的顶点必须恒首节点色（纯红）"
        );
        // 右端纯色段证据：'c' 右列 t≈1 > 1-inset → t_norm=1 → 纯尾色（蓝）。
        let last = inset.last().unwrap();
        assert_eq!(
            (last.r(), last.g(), last.b()),
            (0, 0, 255),
            "t > 1-inset 的顶点必须恒尾节点色（纯蓝）"
        );
        // 对照：inset=0 时 'a' 左列已有少量蓝分量（t 小但 > 0），
        // 证明 inset 确实改变了分布（首顶点从过渡色变纯色）。
        assert!(plain[0].b() > 0, "inset=0 首顶点应已有少量蓝分量");
    }

    /// 回归：渐变参数用文本内坐标而非屏幕绝对坐标。旧实现拿
    /// `frame.x + 字形偏移` 除文本宽再 clamp——滚动弹幕入屏右侧（frame.x
    /// ≈ 视口宽 ≫ 文本宽）与固定弹幕居中（centered_x 数百 px）时恒被夹到
    /// t=1 整条单色，只有弹幕滚到屏幕左端才见渐变（用户截图实锤）。
    /// 修复后渐变顶点色与屏幕位置无关。
    #[test]
    fn mesh_gradient_colors_independent_of_screen_x() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let make_mesh = |x: f64| {
            let mut atlas = GlyphAtlas::new();
            let mut item = test_item("abc", 0xFFFF_FFFFu32 as i32, 24.0);
            item.width = font.measure("abc", 24.0).0;
            item.style = Some(DyStyle {
                nodes: vec![0xFF0000, 0x0000FF],
                target: DyGradientTarget::Fill,
            });
            let frame = test_frame(x, 0.0);
            let items = vec![(frame, &item)];
            let params = MeshParams {
                font: &font,
                outline_profile: 0.0,
                video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
                pixels_per_point: 1.0,
                global_opacity: 1.0,
                texture_id: TextureId::Managed(0),
                gradient_enabled: true,
                gradient_inset: 0.0,
            };
            let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
            mesh.vertices.iter().map(|v| v.color).collect::<Vec<_>>()
        };
        let origin = make_mesh(0.0);
        // 屏幕右端（滚动刚入屏）、居中（固定弹幕典型）、负值（滚出屏）：
        // 三种位置的顶点色序列必须与原点完全一致。
        for x in [600.0, 1280.0, -50.0] {
            let colors = make_mesh(x);
            assert_eq!(
                colors, origin,
                "渐变顶点色不得依赖屏幕 x（x={x} 时应为文本内渐变）"
            );
        }
    }

    #[test]
    fn mesh_gradient_disabled_degrades_to_default_colors() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        // 带 dy 样式但开关关：回归默认配色（描边黑、文本原色）。
        let mut item = test_item("ab", 0xFF00_FF00u32 as i32, 24.0);
        item.style = Some(DyStyle {
            nodes: vec![0xFF0000, 0x0000FF],
            target: DyGradientTarget::Fill,
        });
        let frame = test_frame(0.0, 0.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        let expected = Color32::from_rgba_unmultiplied(0, 255, 0, 255);
        for v in &mesh.vertices {
            assert_eq!(v.color, expected, "开关关 = 原色（忽略 dy 样式）");
        }
    }

    #[test]
    fn mesh_applies_global_opacity_to_vertex_alpha() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let item = test_item("ab", 0xFF00_FF00u32 as i32, 24.0);
        let frame = test_frame(0.0, 0.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 0.5,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        assert_eq!(mesh.indices.len() / 6, 2, "无描边 profile 时仅 fill");
        let expected = Color32::from_rgba_unmultiplied(0, 255, 0, 128);
        for v in &mesh.vertices {
            assert_eq!(v.color, expected);
        }
    }

    #[test]
    fn space_glyph_advances_pen_without_quads() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let item = test_item("a b", 0xFFFF_FFFFu32 as i32, 24.0);
        let frame = test_frame(0.0, 0.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        assert_eq!(mesh.indices.len() / 6, 2, "空格不产生 quad 只推进 pen");
        let space = atlas
            .glyph(font.font(), ' ', 24.0, 0)
            .expect("空格 entry 必须存在（承载 advance）");
        assert_eq!((space.w, space.h), (0, 0));
        assert!(space.advance > 0.0);
    }

    /// M2 验收：同屏峰值（100 条可见 × 双 pass 描边）Mesh 构建耗时应远低于一帧。
    #[test]
    fn mesh_builds_peak_visible_items_quickly() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let owned: Vec<(FrameItem, PreparedItem)> = (0..100)
            .map(|i| {
                (
                    test_frame(0.0, 0.0),
                    test_item(
                        &format!("同屏峰值弹幕内容 {i:03}"),
                        0xFFFF_FFFFu32 as i32,
                        24.0,
                    ),
                )
            })
            .collect();
        let refs: Vec<(FrameItem, &PreparedItem)> =
            owned.iter().map(|(f, i)| (f.clone(), i)).collect();
        let params = MeshParams {
            font: &font,
            outline_profile: 2.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 720.0)),
            pixels_per_point: 1.0,
            global_opacity: 0.8,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        // 首帧含 glyph 栅格化 + 膨胀
        let first = std::time::Instant::now();
        let mesh = build_danmaku_mesh(&mut atlas, &params, &refs);
        let first_elapsed = first.elapsed();
        // 第二帧全缓存命中
        let steady = std::time::Instant::now();
        let mesh2 = build_danmaku_mesh(&mut atlas, &params, &refs);
        let steady_elapsed = steady.elapsed();

        assert!(!mesh.vertices.is_empty());
        assert_eq!(mesh.vertices.len(), mesh2.vertices.len());
        eprintln!(
            "100 visible items mesh: first={first_elapsed:?} steady={steady_elapsed:?} quads={}",
            mesh.indices.len() / 6
        );
        // debug 构建无优化且受并行测试抢占影响，阈值放宽；release 用真实帧预算
        let budget_ms = if cfg!(debug_assertions) { 100 } else { 16 };
        assert!(
            steady_elapsed.as_millis() < budget_ms,
            "稳态 Mesh 构建应低于 {budget_ms}ms，实际 {steady_elapsed:?}"
        );
    }

    #[test]
    fn mode7_frame_alpha_scales_vertex_alpha() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let item = test_item("a", 0xFFFF_FFFFu32 as i32, 24.0);
        let mut frame = test_frame(50.0, 100.0);
        frame.alpha = 0.5; // mode7 渐变 alpha
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        // frame.alpha=0.5 * item.opacity=1.0 * global=1.0 → 128
        for v in &mesh.vertices {
            assert_eq!(v.color.a(), 128, "顶点 alpha 应为 frame.alpha × 255");
        }
    }

    #[test]
    fn mode7_zrotate_rotates_vertices() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let mut item = test_item("abc", 0xFFFF_FFFFu32 as i32, 24.0);
        item.special = Some(crate::normalize::SpecialData {
            x1: 0.0,
            y1: 0.0,
            x2: 0.0,
            y2: 0.0,
            alpha_from: 1.0,
            alpha_to: 1.0,
            duration_s: 1.0,
            z_rotate: std::f32::consts::FRAC_PI_2, // 90°
            date: 0,
        });
        let frame = test_frame(100.0, 100.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        // "abc" 水平排列，旋转 90° 后 y 方向 span 应大于 x 方向 span。
        let xs: Vec<f32> = mesh.vertices.iter().map(|v| v.pos.x).collect();
        let ys: Vec<f32> = mesh.vertices.iter().map(|v| v.pos.y).collect();
        let x_span = xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
            - xs.iter().cloned().fold(f32::INFINITY, f32::min);
        let y_span = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
            - ys.iter().cloned().fold(f32::INFINITY, f32::min);
        assert!(
            y_span > x_span,
            "90° 旋转后 y span({y_span}) 应大于 x span({x_span})"
        );
    }

    #[test]
    fn multiline_renders_second_line_below_first() {
        let Some(font) = sys_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let item = test_item("a\nb", 0xFFFF_FFFFu32 as i32, 24.0);
        let frame = test_frame(10.0, 20.0);
        let items = vec![(frame, &item)];
        let params = MeshParams {
            font: &font,
            outline_profile: 0.0,
            video_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0)),
            pixels_per_point: 1.0,
            global_opacity: 1.0,
            texture_id: TextureId::Managed(0),
            gradient_enabled: false,
            gradient_inset: 0.0,
        };
        let mesh = build_danmaku_mesh(&mut atlas, &params, &items);
        // outline_profile=0 → 仅 fill pass：2 字符 = 2 quad = 8 顶点。
        // fill 'a' = vertices[0..4]（第一行），fill 'b' = vertices[4..8]（第二行）。
        assert!(mesh.vertices.len() >= 8, "至少 2 fill quad");
        let first_y = mesh.vertices[0].pos.y;
        let second_y = mesh.vertices[4].pos.y;
        assert!(
            second_y > first_y,
            "第二行 y({second_y}) 应大于第一行 y({first_y})"
        );
    }
}
