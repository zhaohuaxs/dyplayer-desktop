//! `mpv_node` 的安全 owned 投影 + 释放 guard。
//!
//! `node_from_raw` 是全 crate 唯一读取裸 `MpvNode` union 的 unsafe 点；
//! 转换出的 [`Node`] 是纯安全数据，[`tracks_from_node`]/[`chapters_from_node`]
//! 在其上做模式匹配。[`NodeGuard`]/[`MpvStrGuard`] 用 Drop 调对应 mpv 释放
//! 函数，保证 panic 路径不泄漏（mpv 失败时不写 out 参数，guard 持 zeroed
//! node 时 free 是 no-op，与历史手工时序等价）。

use std::ffi::{CStr, c_char};
use std::sync::Arc;

use crate::client::{Chapter, Track};
use crate::ffi::{
    MpvLibrary, MpvNode, MpvNodeList, MPV_NODE_FORMAT_ARRAY, MPV_NODE_FORMAT_DOUBLE,
    MPV_NODE_FORMAT_FLAG, MPV_NODE_FORMAT_INT64, MPV_NODE_FORMAT_MAP, MPV_NODE_FORMAT_NONE,
    MPV_NODE_FORMAT_STRING,
};

/// `mpv_node` 的安全 owned 投影（递归）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    None,
    String(String),
    Flag(bool),
    Int64(i64),
    Double(f64),
    Array(Vec<Node>),
    Map(Vec<(String, Node)>),
}

/// 递归把裸 `MpvNode` 转成 owned [`Node`]。全 crate 唯一读取 union 的 unsafe 点。
///
/// 安全前提：`raw` 由 mpv 填充且在调用方释放前保持有效；本函数只读不释放。
/// 防御：null string → `String::new()`；null list → 空集合；num<0 → 0；
/// 未知 format → [`Node::None`]（不 panic）。
pub(crate) unsafe fn node_from_raw(raw: &MpvNode) -> Node {
    unsafe {
        match raw.format {
            MPV_NODE_FORMAT_NONE => Node::None,
            MPV_NODE_FORMAT_STRING => {
                if raw.u.string.is_null() {
                    Node::String(String::new())
                } else {
                    Node::String(CStr::from_ptr(raw.u.string).to_string_lossy().into_owned())
                }
            }
            MPV_NODE_FORMAT_FLAG => Node::Flag(raw.u.flag != 0),
            MPV_NODE_FORMAT_INT64 => Node::Int64(raw.u.int64),
            MPV_NODE_FORMAT_DOUBLE => Node::Double(raw.u.double_),
            MPV_NODE_FORMAT_ARRAY => Node::Array(read_array(raw.u.list)),
            MPV_NODE_FORMAT_MAP => Node::Map(read_map(raw.u.list)),
            _ => Node::None,
        }
    }
}

unsafe fn read_array(list: *mut MpvNodeList) -> Vec<Node> {
    unsafe {
        let mut out = Vec::new();
        if list.is_null() {
            return out;
        }
        let num = (*list).num.max(0) as usize;
        for i in 0..num {
            let item = &*(*list).values.add(i);
            out.push(node_from_raw(item));
        }
        out
    }
}

unsafe fn read_map(list: *mut MpvNodeList) -> Vec<(String, Node)> {
    unsafe {
        let mut out = Vec::new();
        if list.is_null() {
            return out;
        }
        let num = (*list).num.max(0) as usize;
        for i in 0..num {
            let key_ptr = *(*list).keys.add(i);
            let key = if key_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(key_ptr).to_string_lossy().into_owned()
            };
            let val = &*(*list).values.add(i);
            out.push((key, node_from_raw(val)));
        }
        out
    }
}


/// 持有 mpv 分配的 `MpvNode`，Drop 调 `mpv_free_node_contents`。
///
/// 用法：`zeroed` 创建 → `node_ptr` 传给 `mpv_get_property` → `read` 取 owned。
/// 失败路径（`check?` 提前返回）时 guard Drop 对 zeroed node 调 free 是 no-op
/// （mpv 对 `MPV_NODE_FORMAT_NONE` 不释放），与历史"失败不 free"时序等价。
pub(crate) struct NodeGuard {
    node: MpvNode,
    lib: Arc<MpvLibrary>,
}

impl NodeGuard {
    pub(crate) fn zeroed(lib: Arc<MpvLibrary>) -> Self {
        Self {
            node: unsafe { std::mem::zeroed() },
            lib,
        }
    }
    pub(crate) fn node_ptr(&mut self) -> *mut MpvNode {
        &mut self.node
    }
    pub(crate) unsafe fn read(&self) -> Node {
        unsafe { node_from_raw(&self.node) }
    }
}

impl Drop for NodeGuard {
    fn drop(&mut self) {
        unsafe {
            (self.lib.mpv_free_node_contents)(&mut self.node);
        }
    }
}

/// 持有 mpv 分配的 C 字符串，Drop 调 `mpv_free`。
pub(crate) struct MpvStrGuard {
    ptr: *mut c_char,
    lib: Arc<MpvLibrary>,
}

impl MpvStrGuard {
    pub(crate) unsafe fn from_ptr(ptr: *mut c_char, lib: Arc<MpvLibrary>) -> Self {
        Self { ptr, lib }
    }
    pub(crate) fn is_null(&self) -> bool {
        self.ptr.is_null()
    }
    pub(crate) unsafe fn to_string(&self) -> String {
        unsafe { CStr::from_ptr(self.ptr).to_string_lossy().into_owned() }
    }
}

impl Drop for MpvStrGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                (self.lib.mpv_free)(self.ptr as *mut std::ffi::c_void);
            }
        }
    }
}

/// 解析 track-list 节点（ARRAY of MAP {id,type,selected,title,lang}）。
/// 结构不符或为空返回空列表，不视为错误。纯安全函数。
pub(crate) fn tracks_from_node(node: &Node) -> Vec<Track> {
    let Node::Array(items) = node else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Node::Map(map) = item else {
            continue;
        };
        let mut id = 0i64;
        let mut kind = String::new();
        let mut selected = false;
        let mut title = String::new();
        let mut lang = String::new();
        for (key, val) in map {
            match (key.as_str(), val) {
                ("id", Node::Int64(v)) => id = *v,
                ("type", Node::String(s)) => kind = s.clone(),
                ("selected", Node::Flag(f)) => selected = *f,
                ("title", Node::String(s)) => title = s.clone(),
                ("lang", Node::String(s)) => lang = s.clone(),
                _ => {}
            }
        }
        if !kind.is_empty() {
            out.push(Track {
                id,
                kind,
                selected,
                title,
                lang,
            });
        }
    }
    out
}

/// 解析 chapter-list 节点（ARRAY of MAP {title,time}）。
/// 无章节的媒体返回空列表。纯安全函数。
pub(crate) fn chapters_from_node(node: &Node) -> Vec<Chapter> {
    let Node::Array(items) = node else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Node::Map(map) = item else {
            continue;
        };
        let mut title = String::new();
        let mut time = 0.0;
        for (key, val) in map {
            match (key.as_str(), val) {
                ("title", Node::String(s)) => title = s.clone(),
                ("time", Node::Double(v)) => time = *v,
                _ => {}
            }
        }
        out.push(Chapter { title, time });
    }
    out
}
#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;
    use crate::ffi::{
        MpvNode, MpvNodeList, MpvNodeU, MPV_NODE_FORMAT_ARRAY, MPV_NODE_FORMAT_DOUBLE,
        MPV_NODE_FORMAT_FLAG, MPV_NODE_FORMAT_INT64, MPV_NODE_FORMAT_MAP, MPV_NODE_FORMAT_NONE,
        MPV_NODE_FORMAT_STRING,
    };

    fn str_node(s: &CString) -> MpvNode {
        MpvNode {
            u: MpvNodeU {
                string: s.as_ptr() as *mut _,
            },
            format: MPV_NODE_FORMAT_STRING,
        }
    }
    fn map_node(keys: &[&CString], values: &[MpvNode]) -> MpvNode {
        // 测试 helper：把 list + keys + values 装进一个 Box 并泄漏（Box::leak），
        // 保证所有指针在 node_from_raw 调用时有效。CString 由调用方持有。
        let mut keys_vec: Vec<*mut std::ffi::c_char> =
            keys.iter().map(|k| k.as_ptr() as *mut _).collect();
        let mut values_vec: Vec<MpvNode> = values.to_vec();
        let list = MpvNodeList {
            num: values.len() as std::ffi::c_int,
            values: values_vec.as_mut_ptr(),
            keys: keys_vec.as_mut_ptr(),
        };
        let leak: &'static mut (MpvNodeList, Vec<*mut std::ffi::c_char>, Vec<MpvNode>) =
            Box::leak(Box::new((list, keys_vec, values_vec)));
        MpvNode {
            u: MpvNodeU {
                list: &mut leak.0 as *mut MpvNodeList,
            },
            format: MPV_NODE_FORMAT_MAP,
        }
    }
    fn array_node(values: &[MpvNode]) -> MpvNode {
        let mut values_vec: Vec<MpvNode> = values.to_vec();
        let list = MpvNodeList {
            num: values.len() as std::ffi::c_int,
            values: values_vec.as_mut_ptr(),
            keys: std::ptr::null_mut(),
        };
        let leak: &'static mut (MpvNodeList, Vec<MpvNode>) =
            Box::leak(Box::new((list, values_vec)));
        MpvNode {
            u: MpvNodeU {
                list: &mut leak.0 as *mut MpvNodeList,
            },
            format: MPV_NODE_FORMAT_ARRAY,
        }
    }

    #[test]
    fn tracks_from_node_complete() {
        let kind = CString::new("audio").unwrap();
        let title = CString::new("eng").unwrap();
        let lang = CString::new("en").unwrap();
        let k_id = CString::new("id").unwrap();
        let k_type = CString::new("type").unwrap();
        let k_sel = CString::new("selected").unwrap();
        let k_title = CString::new("title").unwrap();
        let k_lang = CString::new("lang").unwrap();
        let id_node = MpvNode {
            u: MpvNodeU { int64: 2 },
            format: MPV_NODE_FORMAT_INT64,
        };
        let sel_node = MpvNode {
            u: MpvNodeU { flag: 1 },
            format: MPV_NODE_FORMAT_FLAG,
        };
        let track = map_node(
            &[&k_id, &k_type, &k_sel, &k_title, &k_lang],
            &[id_node, str_node(&kind), sel_node, str_node(&title), str_node(&lang)],
        );
        let node = Node::Array(vec![unsafe { node_from_raw(&track) }]);
        let tracks = tracks_from_node(&node);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, 2);
        assert_eq!(tracks[0].kind, "audio");
        assert!(tracks[0].selected);
        assert_eq!(tracks[0].title, "eng");
        assert_eq!(tracks[0].lang, "en");
    }

    #[test]
    fn tracks_from_node_wrong_type_discarded() {
        // "type" 字段是 Int64 而非 String → kind 空 → track 丢弃
        let k_type = CString::new("type").unwrap();
        let type_node = MpvNode {
            u: MpvNodeU { int64: 1 },
            format: MPV_NODE_FORMAT_INT64,
        };
        let track = map_node(&[&k_type], &[type_node]);
        let node = Node::Array(vec![unsafe { node_from_raw(&track) }]);
        assert!(tracks_from_node(&node).is_empty());
    }

    #[test]
    fn tracks_from_node_non_array_empty() {
        assert!(tracks_from_node(&Node::None).is_empty());
        assert!(tracks_from_node(&Node::String("x".into())).is_empty());
        assert!(tracks_from_node(&Node::Array(vec![])).is_empty());
    }

    #[test]
    fn chapters_from_node_complete() {
        let title = CString::new("Intro").unwrap();
        let k_title = CString::new("title").unwrap();
        let k_time = CString::new("time").unwrap();
        let time_node = MpvNode {
            u: MpvNodeU { double_: 5.5 },
            format: MPV_NODE_FORMAT_DOUBLE,
        };
        let ch = map_node(&[&k_title, &k_time], &[str_node(&title), time_node]);
        let node = Node::Array(vec![unsafe { node_from_raw(&ch) }]);
        let chapters = chapters_from_node(&node);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title, "Intro");
        assert_eq!(chapters[0].time, 5.5);
    }

    #[test]
    fn chapters_from_node_missing_time_defaults_zero() {
        let title = CString::new("Ch").unwrap();
        let k_title = CString::new("title").unwrap();
        let ch = map_node(&[&k_title], &[str_node(&title)]);
        let node = Node::Array(vec![unsafe { node_from_raw(&ch) }]);
        let chapters = chapters_from_node(&node);
        assert_eq!(chapters[0].time, 0.0);
    }

    #[test]
    fn node_from_raw_scalars() {
        let s = CString::new("hi").unwrap();
        assert_eq!(unsafe { node_from_raw(&str_node(&s)) }, Node::String("hi".into()));
        let null_node = MpvNode {
            u: MpvNodeU {
                string: std::ptr::null_mut(),
            },
            format: MPV_NODE_FORMAT_STRING,
        };
        assert_eq!(unsafe { node_from_raw(&null_node) }, Node::String(String::new()));
        let flag_node = MpvNode {
            u: MpvNodeU { flag: 1 },
            format: MPV_NODE_FORMAT_FLAG,
        };
        assert_eq!(unsafe { node_from_raw(&flag_node) }, Node::Flag(true));
        let int_node = MpvNode {
            u: MpvNodeU { int64: -7 },
            format: MPV_NODE_FORMAT_INT64,
        };
        assert_eq!(unsafe { node_from_raw(&int_node) }, Node::Int64(-7));
        let dbl_node = MpvNode {
            u: MpvNodeU { double_: 2.5 },
            format: MPV_NODE_FORMAT_DOUBLE,
        };
        assert_eq!(unsafe { node_from_raw(&dbl_node) }, Node::Double(2.5));
        let none_node = MpvNode {
            u: MpvNodeU { int64: 0 },
            format: MPV_NODE_FORMAT_NONE,
        };
        assert_eq!(unsafe { node_from_raw(&none_node) }, Node::None);
        let unknown_node = MpvNode {
            u: MpvNodeU { int64: 0 },
            format: 99,
        };
        assert_eq!(unsafe { node_from_raw(&unknown_node) }, Node::None);
    }

    #[test]
    fn node_from_raw_null_list() {
        let arr = MpvNode {
            u: MpvNodeU {
                list: std::ptr::null_mut(),
            },
            format: MPV_NODE_FORMAT_ARRAY,
        };
        assert_eq!(unsafe { node_from_raw(&arr) }, Node::Array(vec![]));
        let map = MpvNode {
            u: MpvNodeU {
                list: std::ptr::null_mut(),
            },
            format: MPV_NODE_FORMAT_MAP,
        };
        assert_eq!(unsafe { node_from_raw(&map) }, Node::Map(vec![]));
    }

    #[test]
    fn node_from_raw_map_and_array() {
        let k = CString::new("a").unwrap();
        let v = CString::new("b").unwrap();
        let map = map_node(&[&k], &[str_node(&v)]);
        let owned = unsafe { node_from_raw(&map) };
        assert_eq!(
            owned,
            Node::Map(vec![("a".into(), Node::String("b".into()))])
        );
        let arr = array_node(&[str_node(&v)]);
        let owned = unsafe { node_from_raw(&arr) };
        assert_eq!(owned, Node::Array(vec![Node::String("b".into())]));
    }

    #[test]
    fn tracks_from_node_via_raw_roundtrip() {
        // 端到端：手工构造 raw track-list → node_from_raw → tracks_from_node
        let k_id = CString::new("id").unwrap();
        let k_type = CString::new("type").unwrap();
        let kind = CString::new("video").unwrap();
        let id_node = MpvNode {
            u: MpvNodeU { int64: 1 },
            format: MPV_NODE_FORMAT_INT64,
        };
        let track = map_node(&[&k_id, &k_type], &[id_node, str_node(&kind)]);
        let raw_list = array_node(&[track]);
        let owned = unsafe { node_from_raw(&raw_list) };
        let tracks = tracks_from_node(&owned);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, 1);
        assert_eq!(tracks[0].kind, "video");
    }
}