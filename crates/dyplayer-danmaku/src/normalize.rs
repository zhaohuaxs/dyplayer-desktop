//! B站/弹弹play 弹幕 XML 规范化（自有层，M2）。
//!
//! 输出 [`NormalizedItem`]（源数据：媒体时间、文本、DFM+ 类型码、ARGB 颜色、源字号）。
//! 布局参数（基准字号缩放、顶底转换、测量宽高）在 layout 层组装 `PrepareItem`
//! 时应用——字号/顶底转换等设置变化只需重新组装 + 后台重 prepare，无需重新解析 XML。
//!
//! 支持两种主流格式（按 `p` 属性字段数自适应，TDD 6.5）：
//! - B站（>=5 字段）：`<d p="stime,mode,fontsize,color,date,pool,uid,rowid">text</d>`
//!   mode 1/2/3=滚动(ScrollRL) 4=底部(FixBottom) 5=顶部(FixTop) 6=逆向(ScrollLR)
//!   7/8/9=高级/代码/BAS（直接丢弃）
//! - 弹弹play（4 字段）：`<d p="stime,mode,color,[uid]">text</d>`（无字号字段，默认 25）
//!
//! 颜色字节序考证：源 color 为十进制 0xRRGGBB（16711680=红、16776960=黄、
//! 16777215=白；B站标志性黄色弹幕 0xFFFF00 按 RGB 解读成立，danmaku2ass 等
//! 主流工具同此约定）。TDD v0.2 6.5 节"十进制 BGR"为笔误，此处按 RGB 实现。

use quick_xml::Reader;
use quick_xml::events::Event;

/// DFM+ 弹幕类型码（vendor/dfm_core/model.rs `DanmakuType`）。
const TYPE_SCROLL_RL: i32 = 1;
const TYPE_FIX_BOTTOM: i32 = 4;
const TYPE_FIX_TOP: i32 = 5;
const TYPE_SCROLL_LR: i32 = 6;
const TYPE_SPECIAL: i32 = 7;

/// B站源字号基准（vendor/typography.rs `DEFAULT_SOURCE_FONT_SIZE`）。
const BILI_BASE_FONT_SIZE: f32 = 25.0;

/// 一条弹幕的规范化源数据（布局无关，可在 session 中长期缓存）。
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedItem {
    pub time_seconds: f64,
    pub text: String,
    pub type_code: i32,
    pub color_argb: i32,
    pub source_font_size: f32,
    /// dy 扩展样式（`<d>` 的 `s` 属性，渐变）；非 dy 格式弹幕为 None。
    pub style: Option<DyStyle>,
    /// mode7 高级弹幕数据（type_code==7 时存在）；其余为 None。
    pub special: Option<SpecialData>,
}

/// mode7 高级弹幕数据（B站 advanced danmaku）。
///
/// 坐标为**原始源坐标**（未还原），布局层 `assemble_prepare_items` 按
/// 视口尺寸与历史基准（`date`）缩放。`date` 还原后保留但不再使用。
/// z_rotate 为弧度（B站源为角度，解析时转换）；实测全 0，兜底用。
#[derive(Debug, Clone, PartialEq)]
pub struct SpecialData {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub alpha_from: f32,
    pub alpha_to: f32,
    pub duration_s: f64,
    pub z_rotate: f32,
    pub date: i64,
}

/// dy 扩展 `s` 属性的渐变目标（第二段）：0 = 描边渐变（文本白）、
/// 1 = 文本渐变（描边黑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DyGradientTarget {
    Outline,
    Fill,
}

/// dy 扩展渐变样式：`s="15880350~3181517,0"`。
/// 节点串为 2-3 个十进制 RGB（同 B站 color 编码），均匀分段渐变；
/// 单节点直接抛弃（降级普通弹幕）。
#[derive(Debug, Clone, PartialEq)]
pub struct DyStyle {
    /// 渐变节点色（2-3 个，0xRRGGBB）。
    pub nodes: Vec<u32>,
    pub target: DyGradientTarget,
}

/// 顶部/底部弹幕转换设置（TDD 设置表：异步重 prepare 参数）。
/// ToScroll/Swap 为上游完整语义保留（remap_top_bottom 消费），
/// 构造方留待设置面板接线，暂无 UI 入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum TopBottomConvert {
    /// 不变。
    #[default]
    Keep,
    /// 顶部/底部转滚动。
    ToScroll,
    /// 顶部与底部互换。
    Swap,
}

/// 对 DFM+ 类型码应用顶部/底部转换重映射。
pub(crate) fn remap_top_bottom(type_code: i32, convert: TopBottomConvert) -> i32 {
    match convert {
        TopBottomConvert::Keep => type_code,
        TopBottomConvert::ToScroll => match type_code {
            TYPE_FIX_BOTTOM | TYPE_FIX_TOP => TYPE_SCROLL_RL,
            other => other,
        },
        TopBottomConvert::Swap => match type_code {
            TYPE_FIX_BOTTOM => TYPE_FIX_TOP,
            TYPE_FIX_TOP => TYPE_FIX_BOTTOM,
            other => other,
        },
    }
}

/// 解析弹幕 XML，返回规范化条目；坏行/高级弹幕/空文本逐条跳过，不整体失败。
///
/// quick-xml 0.38 将实体引用作为独立 `GeneralRef` 事件输出、`xml_content()`
/// 不做实体反转义，故在此手动拼接（数字引用 + 五个 XML 预定义实体）。
/// 不启用 `trim_text`——它会把实体拆分后的每个文本段独立 trim，破坏文本
/// 中间的空格；首尾空白在 [`finish_item`] 统一处理。
pub fn parse_danmaku_xml(xml: &str) -> Vec<NormalizedItem> {
    let mut reader = Reader::from_str(xml);

    let mut items = Vec::new();
    let mut pending_fields: Option<ParsedFields> = None;
    let mut pending_style: Option<DyStyle> = None;
    let mut pending_text = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == b"d" {
                    pending_fields = attributes_p_value(&e).as_deref().and_then(parse_p_fields);
                    pending_style = attributes_s_value(&e).as_deref().and_then(parse_s_style);
                    pending_text.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if pending_fields.is_some()
                    && let Ok(text) = t.xml_content()
                {
                    pending_text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if pending_fields.is_some()
                    && let Some(ch) = resolve_general_ref(&r)
                {
                    pending_text.push(ch);
                }
            }
            Ok(Event::CData(c)) => {
                // R16：CDATA 段原被 `_ => {}` 吞掉，纯/混合 CDATA 弹幕残缺。
                if pending_fields.is_some() {
                    pending_text.push_str(std::str::from_utf8(&c).unwrap_or(""));
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"d"
                    && let Some(fields) = pending_fields.take()
                    && let Some(item) = finish_item(fields, pending_style.take(), &pending_text)
                {
                    items.push(item);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    items
}

/// 解析实体引用：数字引用（`&#123;` / `&#x1F;`）与五个 XML 预定义实体；
/// 未定义实体返回 None 跳过（弹幕文本不含自定义实体）。
fn resolve_general_ref(r: &quick_xml::events::BytesRef) -> Option<char> {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        return Some(ch);
    }
    match r.as_ref() {
        b"lt" => Some('<'),
        b"gt" => Some('>'),
        b"amp" => Some('&'),
        b"apos" => Some('\''),
        b"quot" => Some('"'),
        _ => None,
    }
}

/// `<d>` 标签的解析中间态（字段语义随字段数自适应）。
struct ParsedFields {
    stime: f64,
    mode: i64,
    font_size: f32,
    color: Option<i64>,
    date: Option<i64>,
}

fn attributes_p_value(e: &quick_xml::events::BytesStart) -> Option<String> {
    for attr in e.attributes() {
        let attr = attr.ok()?;
        if attr.key.as_ref() == b"p" {
            return Some(String::from_utf8_lossy(attr.value.as_ref()).into_owned());
        }
    }
    None
}

/// 提取 `<d>` 的 `s` 属性值（dy 扩展样式；无该属性返回 None）。
fn attributes_s_value(e: &quick_xml::events::BytesStart) -> Option<String> {
    for attr in e.attributes() {
        let attr = attr.ok()?;
        if attr.key.as_ref() == b"s" {
            return Some(String::from_utf8_lossy(attr.value.as_ref()).into_owned());
        }
    }
    None
}

/// 解析 dy 扩展 `s` 属性：`"rgb~rgb[,~rgb],target"`。
/// 节点 2-3 个（单节点抛弃）、每节点十进制 RGB、target 0|1；
/// 任何不合法形态返回 None（降级普通弹幕，不整体失败）。
fn parse_s_style(s: &str) -> Option<DyStyle> {
    let (nodes_str, target_str) = s.split_once(',')?;
    let target = match target_str.trim() {
        "0" => DyGradientTarget::Outline,
        "1" => DyGradientTarget::Fill,
        _ => return None,
    };
    let parts: Vec<&str> = nodes_str.split('~').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut nodes = Vec::with_capacity(parts.len());
    for part in parts {
        nodes.push(part.trim().parse::<u32>().ok()?);
    }
    Some(DyStyle { nodes, target })
}

/// 按 `p` 字段数自适应解析（TDD 6.5：B站 >=5 字段 / 弹弹play 4 字段）。
fn parse_p_fields(p: &str) -> Option<ParsedFields> {
    let fields: Vec<&str> = p.split(',').map(str::trim).collect();
    match fields.len() {
        n if n >= 5 => Some(ParsedFields {
            stime: fields[0].parse().ok()?,
            mode: fields[1].parse().ok()?,
            font_size: parse_font_size(fields[2]),
            color: fields[3].parse().ok(),
            date: fields.get(4).and_then(|s| s.parse().ok()),
        }),
        4 => Some(ParsedFields {
            stime: fields[0].parse().ok()?,
            mode: fields[1].parse().ok()?,
            font_size: BILI_BASE_FONT_SIZE,
            color: fields[2].parse().ok(),
            date: None,
        }),
        _ => None,
    }
}

/// 源字号上限（R06）：异常大字号可致栅格化数百 MB mask，在解析边界拒绝。
/// B站常规 18-36，200 已覆盖极端自定义。
const MAX_SOURCE_FONT_SIZE: f32 = 200.0;

fn parse_font_size(raw: &str) -> f32 {
    raw.parse::<f32>()
        .ok()
        .filter(|&size| size > 0.0 && size.is_finite() && size <= MAX_SOURCE_FONT_SIZE)
        .unwrap_or(BILI_BASE_FONT_SIZE)
}

/// B站 mode -> DFM+ 类型码；mode7 高级弹幕返回 Special，代码/BAS（8/9）丢弃。
fn bili_mode_to_type_code(mode: i64) -> Option<i32> {
    match mode {
        1..=3 => Some(TYPE_SCROLL_RL),
        4 => Some(TYPE_FIX_BOTTOM),
        5 => Some(TYPE_FIX_TOP),
        6 => Some(TYPE_SCROLL_LR),
        7 => Some(TYPE_SPECIAL),
        _ => None,
    }
}

/// 源颜色（十进制 0xRRGGBB）-> ARGB；非法/缺失（含 0 之外的负数与超范围）回白色。
fn source_color_to_argb(color: Option<i64>) -> i32 {
    match color {
        Some(c @ 0..=0xFF_FFFF) => (0xFF00_0000u32 | c as u32) as i32,
        _ => 0xFF_FF_FF_FFu32 as i32,
    }
}

fn finish_item(
    fields: ParsedFields,
    style: Option<DyStyle>,
    text: &str,
) -> Option<NormalizedItem> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let type_code = bili_mode_to_type_code(fields.mode)?;
    // mode7：text 是 JSON 数组。以 `[` 开头 → 解析高级弹幕；否则退化为 FixTop 普通文本。
    let (text, type_code, special) = if type_code == TYPE_SPECIAL && text.starts_with('[') {
        match parse_mode7_json(text, fields.date) {
            Some((parsed_text, sp)) => (parsed_text, TYPE_SPECIAL, Some(sp)),
            None => return None,
        }
    } else {
        (text.to_string(), if type_code == TYPE_SPECIAL { TYPE_FIX_TOP } else { type_code }, None)
    };
    let text = text.trim().replace("/n", "\n");
    if text.is_empty() {
        return None;
    }
    Some(NormalizedItem {
        time_seconds: fields.stime,
        text,
        type_code,
        color_argb: source_color_to_argb(fields.color),
        source_font_size: fields.font_size,
        style,
        special,
    })
}

/// 解析 mode7 JSON 数组 → (文本, SpecialData)。
///
/// 两种格式：
/// - 新（>=8 元素）：`[x1,y1,"aFrom-aTo",dur,text,zRot,yRot,x2,y2,...]`
/// - 旧（5-7 元素）：`[x1,y1,"aFrom-aTo",dur,text(,x2,y2)?]`（无旋转）
///
/// 坐标/数值可能是数字或字符串（`"50"`）；alpha 为 `"from-to"`；zRotate 源为角度转弧度。
fn parse_mode7_json(raw: &str, date: Option<i64>) -> Option<(String, SpecialData)> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    let elems = split_json_elements(inner);
    if elems.len() < 5 {
        return None;
    }
    let x1 = parse_json_num(&elems[0])?;
    let y1 = parse_json_num(&elems[1])?;
    let (alpha_from, alpha_to) = parse_alpha_field(&elems[2])?;
    let duration_s = parse_json_num(&elems[3])?;
    let text = parse_json_str(&elems[4])?;
    let (z_rotate_deg, x2, y2) = if elems.len() >= 9 {
        let zr = parse_json_num(&elems[5]).unwrap_or(0.0);
        (zr, parse_json_num(&elems[7])?, parse_json_num(&elems[8])?)
    } else if elems.len() >= 7 {
        (0.0, parse_json_num(&elems[5])?, parse_json_num(&elems[6])?)
    } else {
        (0.0, x1, y1)
    };
    let z_rotate = (z_rotate_deg as f32) * std::f32::consts::PI / 180.0;
    Some((
        text,
        SpecialData {
            x1,
            y1,
            x2,
            y2,
            alpha_from,
            alpha_to,
            duration_s: duration_s.max(0.1),
            z_rotate,
            date: date.unwrap_or(0),
        },
    ))
}

/// 按逗号分割 JSON 数组元素，处理字符串内的逗号与转义引号。
fn split_json_elements(inner: &str) -> Vec<String> {
    let mut elems = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escape = false;
    for ch in inner.chars() {
        if in_str {
            cur.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
        } else {
            match ch {
                '"' => {
                    cur.push(ch);
                    in_str = true;
                }
                ',' => {
                    elems.push(cur.trim().to_string());
                    cur.clear();
                }
                _ => cur.push(ch),
            }
        }
    }
    let last = cur.trim().to_string();
    if !last.is_empty() {
        elems.push(last);
    }
    elems
}

/// 解析 JSON 数值（兼容裸数字与字符串包裹的数字 `"50"`）。
fn parse_json_num(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(s);
    s.parse::<f64>().ok()
}

/// 解析 JSON 字符串（去引号、处理 `\"` `\\` `\n` 转义）。
fn parse_json_str(s: &str) -> Option<String> {
    let s = s.trim();
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// 解析 alpha 字段 `"from-to"` → (from, to)。
fn parse_alpha_field(s: &str) -> Option<(f32, f32)> {
    let s = s.trim();
    let s = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(s);
    let (from, to) = s.split_once('-')?;
    Some((from.parse().ok()?, to.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(body: &str) -> String {
        format!(r#"<?xml version="1.0" encoding="utf-8"?><i>{body}</i>"#)
    }

    #[test]
    fn parse_bili_standard_scroll() {
        let xml = wrap(
            r#"<d p="1.500,1,25,16777215,1651749038,0,0,7c9e28d,4465604562751488">你好世界</d>"#,
        );
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.time_seconds, 1.5);
        assert_eq!(item.text, "你好世界");
        assert_eq!(item.type_code, 1);
        assert_eq!(item.color_argb as u32, 0xFFFF_FFFF);
        assert_eq!(item.source_font_size, 25.0);
        assert!(item.style.is_none());
    }

    #[test]
    fn parse_s_style_shapes() {
        // 两节点 + 描边目标（smoke 样本主流形态）。
        assert_eq!(
            parse_s_style("15880350~3181517,0"),
            Some(DyStyle {
                nodes: vec![15880350, 3181517],
                target: DyGradientTarget::Outline,
            })
        );
        // 三节点 + 文本目标。
        assert_eq!(
            parse_s_style("15880350~3181517~15880350,1"),
            Some(DyStyle {
                nodes: vec![15880350, 3181517, 15880350],
                target: DyGradientTarget::Fill,
            })
        );
        // 单节点：抛弃（降级普通弹幕）。
        assert_eq!(parse_s_style("15880350,0"), None);
        // 四节点：超出格式上限，抛弃。
        assert_eq!(parse_s_style("1~2~3~4,0"), None);
        // 坏 target / 非数字节点 / 缺逗号 / 空。
        assert_eq!(parse_s_style("1~2,2"), None);
        assert_eq!(parse_s_style("1~x,0"), None);
        assert_eq!(parse_s_style("1~2"), None);
        assert_eq!(parse_s_style(""), None);
    }

    #[test]
    fn parse_dy_extended_style_attribute() {
        let xml = wrap(concat!(
            r#"<d p="0.1,1,25,16777215,1519309211000,0,834399294,659120950" s="15880350~3181517,1">哈哈哈哈</d>"#,
            r#"<d p="0.0,1,25,16777215,1468473554000,0,949354991,77213938">帅哥帅了</d>"#,
            r#"<d p="0.0,1,25,16777215,1519640390000,0,384906045,662663741" s="15880350,0">武道馆</d>"#,
            r#"<d p="0.0,1,25,16777215,1521455865000,0,720273109,678696961" s="bad">ds</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 4);
        // 合法 dy 样式：透传。
        assert_eq!(
            items[0].style,
            Some(DyStyle {
                nodes: vec![15880350, 3181517],
                target: DyGradientTarget::Fill,
            })
        );
        // 无 s 属性：普通弹幕。
        assert!(items[1].style.is_none());
        // 单节点 / 坏样式：过滤降级，弹幕本体保留。
        assert!(items[2].style.is_none());
        assert!(items[3].style.is_none());
        assert_eq!(items[3].text, "ds");
    }

    /// smoke 实弹样本（XNDI4OTYwMTg0 调色弹幕，2960 条 / 1802 带 s）：
    /// dy 扩展全量解析不丢弹幕、样式形态与实测分布一致。
    #[test]
    fn parse_smoke_dy_extended_sample() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../smoke/XNDI4OTYwMTg0-调色-2-1-20251229150441.xml"
        );
        let Ok(xml) = std::fs::read_to_string(path) else {
            eprintln!("skip: smoke sample not present");
            return;
        };
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 2960, "全量解析不丢弹幕");
        let styled = items.iter().filter(|i| i.style.is_some()).count();
        assert_eq!(styled, 1802, "s 属性合法样本全透传");
        // 两种已知形态各就各位。
        let outline_count = items
            .iter()
            .filter(|i| {
                i.style
                    .as_ref()
                    .is_some_and(|s| s.target == DyGradientTarget::Outline)
            })
            .count();
        let fill_count = items
            .iter()
            .filter(|i| {
                i.style
                    .as_ref()
                    .is_some_and(|s| s.target == DyGradientTarget::Fill)
            })
            .count();
        assert_eq!(outline_count, 1790, "target=0 样本（904+886）");
        assert_eq!(fill_count, 12, "target=1 样本（8+4）");
    }

    #[test]
    fn parse_bili_all_supported_modes() {
        let xml = wrap(concat!(
            r#"<d p="0,1,25,0,0,0,0,0,0">a</d>"#,
            r#"<d p="0,2,25,0,0,0,0,0,0">b</d>"#,
            r#"<d p="0,3,25,0,0,0,0,0,0">c</d>"#,
            r#"<d p="0,4,25,0,0,0,0,0,0">d</d>"#,
            r#"<d p="0,5,25,0,0,0,0,0,0">e</d>"#,
            r#"<d p="0,6,25,0,0,0,0,0,0">f</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        let codes: Vec<i32> = items.iter().map(|i| i.type_code).collect();
        assert_eq!(codes, vec![1, 1, 1, 4, 5, 6]);
    }

    #[test]
    fn parse_bili_drops_special_modes() {
        let xml = wrap(concat!(
            r#"<d p="0,7,25,0,0,0,0,0,0">高级纯文本退化</d>"#,
            r#"<d p="0,8,25,0,0,0,0,0,0">代码</d>"#,
            r#"<d p="0,9,25,0,0,0,0,0,0">BAS</d>"#,
            r#"<d p="0,1,25,0,0,0,0,0,0">ok</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        // mode7 纯文本退化为 FixTop；mode 8/9 丢弃。
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "高级纯文本退化");
        assert_eq!(items[0].type_code, TYPE_FIX_TOP);
        assert_eq!(items[1].text, "ok");
    }

    #[test]
    fn parse_bili_font_sizes_kept() {
        let xml = wrap(concat!(
            r#"<d p="0,1,18,0,0,0,0,0,0">小</d>"#,
            r#"<d p="0,1,25,0,0,0,0,0,0">标准</d>"#,
            r#"<d p="0,1,36,0,0,0,0,0,0">大</d>"#,
            r#"<d p="0,1,0,0,0,0,0,0,0">非法回基准</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        let sizes: Vec<f32> = items.iter().map(|i| i.source_font_size).collect();
        assert_eq!(sizes, vec![18.0, 25.0, 36.0, 25.0]);
    }

    #[test]
    fn parse_dandanplay_four_fields() {
        let xml = wrap(r#"<d p="2.0,4,255,42">底部弹幕</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.time_seconds, 2.0);
        assert_eq!(item.type_code, 4);
        assert_eq!(item.color_argb as u32, 0xFF00_00FF);
        assert_eq!(item.source_font_size, 25.0);
    }

    #[test]
    fn parse_skips_short_field_rows() {
        let xml = wrap(concat!(
            r#"<d p="1.0,1,25">三字段</d>"#,
            r#"<d p="1.0">一字段</d>"#,
            r#"<d p="0,1,25,0,0,0,0,0,0">正常</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "正常");
    }

    #[test]
    fn parse_unescapes_entities() {
        let xml = wrap(r#"<d p="0,1,25,0,0,0,0,0,0">&lt;b&gt;a &amp; b&lt;/b&gt;</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "<b>a & b</b>");
    }

    #[test]
    fn parse_unescapes_char_refs_and_keeps_inner_spaces() {
        let xml = wrap(
            r#"<d p="0,1,25,0,0,0,0,0,0">A &#x4F60;&#22909; &quot;D&quot; &#65;&#66;&#67;</d>"#,
        );
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "A 你好 \"D\" ABC");
    }

    #[test]
    fn parse_drops_empty_text() {
        let xml = wrap(concat!(
            r#"<d p="0,1,25,0,0,0,0,0,0">  </d>"#,
            r#"<d p="0,1,25,0,0,0,0,0,0"/>"#,
            r#"<d p="0,1,25,0,0,0,0,0,0">有内容</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "有内容");
    }

    #[test]
    fn parse_bad_rows_tolerated() {
        let xml = wrap(concat!(
            r#"<d p="bad,1,25,0,0,0,0,0,0">坏 stime</d>"#,
            r#"<d p="1.0,x,25,0,0,0,0,0,0">坏 mode</d>"#,
            r#"<d>没有 p 属性</d>"#,
            r#"<d p="1.0,1,bad,notanumber,0,0,0,0">坏字号和颜色</d>"#,
            r#"<d p="2.0,1,25,0,0,0,0,0,0">好行</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "坏字号和颜色");
        assert_eq!(items[0].color_argb as u32, 0xFFFF_FFFF);
        assert_eq!(items[0].source_font_size, 25.0);
        assert_eq!(items[1].text, "好行");
    }

    #[test]
    fn parse_tolerates_metadata_block() {
        let xml = wrap(concat!(
            r#"<metadata><tenant_name>录播姬</tenant_name><record_time>2022-06-01T00:00:00+08:00</record_time></metadata>"#,
            r#"<d p="0,1,25,16777215,0,0,0,0,0">好行</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "好行");
    }

    #[test]
    fn color_mapping_table() {
        let cases: [(i64, u32); 5] = [
            (16_777_215, 0xFFFF_FFFF), // 白
            (16_711_680, 0xFFFF_0000), // 红
            (16_776_960, 0xFFFF_FF00), // 黄
            (255, 0xFF00_00FF),        // 蓝
            (0, 0xFF00_0000),          // 黑（合法）
        ];
        for (source, argb) in cases {
            assert_eq!(
                source_color_to_argb(Some(source)) as u32,
                argb,
                "source={source}"
            );
        }
        assert_eq!(source_color_to_argb(None) as u32, 0xFFFF_FFFF);
        assert_eq!(source_color_to_argb(Some(-1)) as u32, 0xFFFF_FFFF);
        assert_eq!(source_color_to_argb(Some(0x100_0000)) as u32, 0xFFFF_FFFF);
    }

    #[test]
    fn remap_top_bottom_modes() {
        assert_eq!(remap_top_bottom(1, TopBottomConvert::Keep), 1);
        assert_eq!(remap_top_bottom(4, TopBottomConvert::Keep), 4);
        assert_eq!(remap_top_bottom(4, TopBottomConvert::ToScroll), 1);
        assert_eq!(remap_top_bottom(5, TopBottomConvert::ToScroll), 1);
        assert_eq!(remap_top_bottom(1, TopBottomConvert::ToScroll), 1);
        assert_eq!(remap_top_bottom(6, TopBottomConvert::ToScroll), 6);
        assert_eq!(remap_top_bottom(4, TopBottomConvert::Swap), 5);
        assert_eq!(remap_top_bottom(5, TopBottomConvert::Swap), 4);
        assert_eq!(remap_top_bottom(1, TopBottomConvert::Swap), 1);
        assert_eq!(remap_top_bottom(6, TopBottomConvert::Swap), 6);
    }

    #[test]
    fn parse_empty_xml_returns_empty() {
        assert!(parse_danmaku_xml("").is_empty());
        assert!(parse_danmaku_xml("not xml at all").is_empty());
    }

    #[test]
    fn parse_mode7_new_format_14_elements() {
        let xml = wrap(r#"<d p="0.096,7,36,16711680,1371211686,0,x,0">[29,108,"1-1",10,"百万播放",0,0,29,108,500,0,false,"黑体",1]</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.type_code, 7);
        assert_eq!(it.text, "百万播放");
        let sp = it.special.as_ref().expect("special");
        assert_eq!(sp.x1, 29.0);
        assert_eq!(sp.y1, 108.0);
        assert_eq!(sp.x2, 29.0);
        assert_eq!(sp.y2, 108.0);
        assert!((sp.alpha_from - 1.0).abs() < 1e-6);
        assert!((sp.alpha_to - 1.0).abs() < 1e-6);
        assert!((sp.duration_s - 10.0).abs() < 1e-6);
        assert!(sp.z_rotate.abs() < 1e-6);
        assert_eq!(sp.date, 1371211686);
    }

    #[test]
    fn parse_mode7_old_format_7_elements() {
        let xml = wrap(r#"<d p="0.1,7,36,16751001,1291190667,0,x,0">["50","245","0.1-1.00","4.50","恭喜达成","350","0"]</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        let sp = items[0].special.as_ref().expect("special");
        assert_eq!(sp.x1, 50.0);
        assert_eq!(sp.y1, 245.0);
        assert_eq!(sp.x2, 350.0);
        assert_eq!(sp.y2, 0.0);
        assert!((sp.alpha_from - 0.1).abs() < 1e-6);
        assert!((sp.alpha_to - 1.0).abs() < 1e-6);
        assert!((sp.duration_s - 4.5).abs() < 1e-6);
    }

    #[test]
    fn parse_mode7_old_format_5_elements_same_pos() {
        let xml = wrap(r#"<d p="7.2,7,64,16777215,1293894704,0,x,0">["241","150","1.00-1.00","7","酱"]</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        let sp = items[0].special.as_ref().expect("special");
        assert_eq!(sp.x1, 241.0);
        assert_eq!(sp.y1, 150.0);
        assert_eq!(sp.x2, 241.0, "5 元素起止同位");
        assert_eq!(sp.y2, 150.0);
    }

    #[test]
    fn parse_mode7_plain_text_degrades_to_fixtop() {
        let xml = wrap(r#"<d p="3.2,7,25,16777215,1310170105,0,x,0">/n/n/n/n ~~BILIBILI</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].type_code, TYPE_FIX_TOP);
        assert!(items[0].special.is_none());
        assert_eq!(items[0].text, "\n\n\n\n ~~BILIBILI");
    }

    #[test]
    fn newline_replaces_slash_n_in_text() {
        let xml = wrap(r#"<d p="0,4,25,16777215,0,0,0,0,0">hello/nworld</d>"#);
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "hello\nworld");
    }

    #[test]
    fn parse_mode7_av314_real_data_mixed_formats() {
        // AV314 电磁炮 P1 真实 m7 弹幕形态（新格式 14 元素 + 旧格式 7/5 元素 + 纯文本退化）。
        let xml = wrap(concat!(
            r#"<d p="0.096,7,36,16711680,1371211686,0,e,0">[29,108,"1-1",10,"百万播放",0,0,29,108,500,0,false,"黑体",1]</d>"#,
            r#"<d p="0.1,7,36,16751001,1291190667,0,e,0">["50","245","0.1-1.00","4.50","恭喜十万","350","0"]</d>"#,
            r#"<d p="3.2,7,25,10053222,1310170106,0,e,0">- -</d>"#,
            r#"<d p="3.2,7,25,16777215,1310170105,0,e,0">/n/n ~~BILIBILI</d>"#,
            r#"<d p="3.4,7,45,16711782,1291190674,0,e,0">["3","131","0.1-1.00","3.5","十一万"]</d>"#,
            r#"<d p="83.964,7,53,13382553,1670042265,0,e,0">[0,0,"1-1",4.5,"bilibili",0,0,0,0,500,0,true,"黑体",1]</d>"#,
        ));
        let items = parse_danmaku_xml(&xml);
        assert_eq!(items.len(), 6, "6 条全部解析（2 纯文本退化 + 4 JSON）");
        let fixtop = items.iter().filter(|i| i.type_code == TYPE_FIX_TOP).count();
        assert_eq!(fixtop, 2, "2 条纯文本退化为 FixTop");
        let special = items.iter().filter(|i| i.type_code == TYPE_SPECIAL).count();
        assert_eq!(special, 4, "4 条 JSON 为 Special");
        for it in items.iter().filter(|i| i.type_code == TYPE_SPECIAL) {
            assert!(it.special.is_some(), "type7 应有 special 数据");
            assert!(!it.text.is_empty(), "文本应从 JSON 提取");
        }
    }
}
