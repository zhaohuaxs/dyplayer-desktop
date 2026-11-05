#![allow(dead_code)]
// FFI 常量表对照 mpv 头文件保持完整；个别暂未消费的常量不裁剪。

use std::ffi::{CStr, CString, c_char, c_double, c_int, c_ulong, c_void};
use std::sync::Arc;

use libloading::{Library, Symbol};
use thiserror::Error;

pub const MPV_FORMAT_NONE: c_int = 0;
pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_FLAG: c_int = 3;
pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_FORMAT_DOUBLE: c_int = 5;
/// 属性 API 格式：取 mpv_node（复合结构，需 mpv_free_node_contents 释放）。
pub const MPV_FORMAT_NODE: c_int = 6;
/// mpv_node.format 值（节点内部格式，与属性 API 格式常量不同）。
pub const MPV_NODE_FORMAT_NONE: c_int = 0;
pub const MPV_NODE_FORMAT_STRING: c_int = 1;
pub const MPV_NODE_FORMAT_FLAG: c_int = 3;
pub const MPV_NODE_FORMAT_INT64: c_int = 4;
pub const MPV_NODE_FORMAT_DOUBLE: c_int = 5;
pub const MPV_NODE_FORMAT_ARRAY: c_int = 7;
pub const MPV_NODE_FORMAT_MAP: c_int = 8;

pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_LOG_MESSAGE: c_int = 2;
pub const MPV_EVENT_START_FILE: c_int = 6;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;
pub const MPV_EVENT_AUDIO_RECONFIG: c_int = 18;
pub const MPV_EVENT_SEEK: c_int = 20;
pub const MPV_EVENT_PLAYBACK_RESTART: c_int = 21;
pub const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

pub const MPV_END_FILE_REASON_EOF: c_int = 0;
pub const MPV_END_FILE_REASON_STOP: c_int = 2;
pub const MPV_END_FILE_REASON_QUIT: c_int = 3;
pub const MPV_END_FILE_REASON_ERROR: c_int = 4;
pub const MPV_END_FILE_REASON_REDIRECT: c_int = 5;

pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
/// 查询下一帧信息（get_info 用），返回 MpvRenderFrameInfo。
pub const MPV_RENDER_PARAM_NEXT_FRAME_INFO: c_int = 11;
/// render 时是否等到 VO 的呈现时机才返回（vo_libmpv.c flip_page 握手）；
/// 不传时 mpv 默认 1。传 0 时 render 立即返回，时序由调用方自负。
pub const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
/// 只取走帧不实际渲染（重复帧可跳过 SW 缩放，buffer 保持上一帧）。
pub const MPV_RENDER_PARAM_SKIP_RENDERING: c_int = 13;
pub const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
pub const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
pub const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
pub const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

pub const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
pub const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
pub const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
/// 开启高级控制（next_frame_info / block_for_target_time / skip_rendering 所需）。
/// render.h:287 = 10（R17：原误用 9=WL_DISPLAY，传 int* 当 wayland display 指针）。
pub const MPV_RENDER_PARAM_ADVANCED_CONTROL: c_int = 10;

pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

pub const MPV_RENDER_API_TYPE_SW: &[u8] = b"sw\0";
pub const MPV_RENDER_API_TYPE_SW_FORMAT_RGB0: &[u8] = b"rgb0\0";
pub const MPV_RENDER_API_TYPE_OPENGL: &[u8] = b"opengl\0";

#[repr(C)]
pub struct MpvOpenGLInitParams {
    pub get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    pub get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
pub struct MpvOpenGLFbo {
    pub fbo: c_int,
    pub w: c_int,
    pub h: c_int,
    pub internal_format: c_int,
}

#[repr(C)]
pub struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvEventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

/// mpv_node：复合属性值（chapter-list 等）。
/// union 字段按 format 选择有效成员，读取前必须判别 format。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MpvNode {
    pub u: MpvNodeU,
    pub format: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union MpvNodeU {
    pub string: *mut c_char,
    pub flag: c_int,
    pub int64: i64,
    pub double_: f64,
    pub list: *mut MpvNodeList,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MpvNodeList {
    pub num: c_int,
    pub values: *mut MpvNode,
    /// 仅 NODE_MAP 有效；ARRAY 时为 null。
    pub keys: *mut *mut c_char,
}

#[repr(C)]
pub struct MpvRenderParam {
    pub kind: c_int,
    pub data: *mut c_void,
}

/// MPV_RENDER_PARAM_NEXT_FRAME_INFO 返回的下一帧信息（render.h
/// mpv_render_frame_info：flags 位集 + 预定呈现时刻）。
#[repr(C)]
pub struct MpvRenderFrameInfo {
    /// MPV_RENDER_FRAME_INFO_{PRESENT=1, REDRAW=2, REPEAT=4, BLOCK_VSYNC=8}
    pub flags: u64,
    /// 预定呈现时刻（mp_time_ns 单位；vsync 锁定时可为 0）
    pub target_time: i64,
}

#[repr(C)]
pub struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct MpvEventLogMessage {
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
    pub log_level: c_int,
}


pub type MpvHandle = c_void;
pub type MpvRenderContextHandle = c_void;

pub type MpvCreateFn = unsafe extern "C" fn() -> *mut MpvHandle;
pub type MpvInitializeFn = unsafe extern "C" fn(*mut MpvHandle) -> c_int;
pub type MpvTerminateDestroyFn = unsafe extern "C" fn(*mut MpvHandle);
/// `(name, value)` 字符串对 set API 签名：`mpv_set_option_string` 与
/// `mpv_set_property_string` 同型——语义不同（前者限 `mpv_initialize`
/// 之前，后者任意时刻），C 签名一致。
pub type MpvSetStringFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int;
pub type MpvCommandFn = unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int;
pub type MpvSetPropertyFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
pub type MpvGetPropertyFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
pub type MpvFreeNodeContentsFn = unsafe extern "C" fn(*mut MpvNode);
pub type MpvObservePropertyFn =
    unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int;
pub type MpvRequestLogMessagesFn =
    unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> c_int;
pub type MpvWaitEventFn = unsafe extern "C" fn(*mut MpvHandle, c_double) -> *mut MpvEvent;
pub type MpvErrorStringFn = unsafe extern "C" fn(c_int) -> *const c_char;
pub type MpvFreeFn = unsafe extern "C" fn(*mut c_void);
pub type MpvRenderContextCreateFn = unsafe extern "C" fn(
    *mut *mut MpvRenderContextHandle,
    *mut MpvHandle,
    *mut MpvRenderParam,
) -> c_int;
pub type MpvRenderContextSetUpdateCallbackFn = unsafe extern "C" fn(
    *mut MpvRenderContextHandle,
    Option<unsafe extern "C" fn(*mut c_void)>,
    *mut c_void,
);
pub type MpvRenderContextUpdateFn = unsafe extern "C" fn(*mut MpvRenderContextHandle) -> u64;
pub type MpvRenderContextGetInfoFn =
    unsafe extern "C" fn(*mut MpvRenderContextHandle, MpvRenderParam) -> c_int;
pub type MpvRenderContextRenderFn =
    unsafe extern "C" fn(*mut MpvRenderContextHandle, *mut MpvRenderParam) -> c_int;
pub type MpvRenderContextReportSwapFn =
    unsafe extern "C" fn(*mut MpvRenderContextHandle);
pub type MpvRenderContextFreeFn = unsafe extern "C" fn(*mut MpvRenderContextHandle);
pub type MpvClientApiVersionFn = unsafe extern "C" fn() -> c_ulong;
pub type MpvGetTimeUsFn = unsafe extern "C" fn(*mut MpvHandle) -> i64;

/// 所需最低 libmpv client API 版本 (major, minor)。
/// (2,1) 对应 mpv 0.35：render SW 参数 SKIP_RENDERING / NEXT_FRAME_INFO 所需。
pub const REQUIRED_API_VERSION: (u32, u32) = (2, 1);

pub struct MpvLibrary {
    _lib: Library,
    pub mpv_create: MpvCreateFn,
    pub mpv_initialize: MpvInitializeFn,
    pub mpv_terminate_destroy: MpvTerminateDestroyFn,
    pub mpv_set_option_string: MpvSetStringFn,
    pub mpv_set_property_string: MpvSetStringFn,
    pub mpv_command: MpvCommandFn,
    pub mpv_set_property: MpvSetPropertyFn,
    pub mpv_get_property: MpvGetPropertyFn,
    pub mpv_free_node_contents: MpvFreeNodeContentsFn,
    pub mpv_observe_property: MpvObservePropertyFn,
    pub mpv_request_log_messages: MpvRequestLogMessagesFn,
    pub mpv_wait_event: MpvWaitEventFn,
    pub mpv_error_string: MpvErrorStringFn,
    pub mpv_free: MpvFreeFn,
    pub mpv_render_context_create: MpvRenderContextCreateFn,
    pub mpv_render_context_set_update_callback: MpvRenderContextSetUpdateCallbackFn,
    pub mpv_render_context_update: MpvRenderContextUpdateFn,
    pub mpv_render_context_get_info: MpvRenderContextGetInfoFn,
    pub mpv_render_context_render: MpvRenderContextRenderFn,
    pub mpv_render_context_report_swap: MpvRenderContextReportSwapFn,
    pub mpv_render_context_free: MpvRenderContextFreeFn,
    pub mpv_client_api_version: MpvClientApiVersionFn,
    pub mpv_get_time_us: MpvGetTimeUsFn,
    /// 实际 libmpv client API 版本 (major, minor)，load 时填入。
    pub api_version: (u32, u32),
}

#[derive(Debug, Error)]
pub enum FfiError {
    #[error("libmpv dll not found in any candidate path: {0}")]
    DllNotFound(String),
    #[error("failed to load libmpv dll {path}: {source}")]
    DllLoad {
        path: String,
        source: libloading::Error,
    },
    #[error("symbol {symbol} missing from libmpv")]
    MissingSymbol { symbol: &'static str },
    #[error("mpv error {code}: {message}")]
    Mpv { code: i32, message: String },
    #[error("libmpv client API too old: found {found:?}, need {need:?}")]
    ApiTooOld {
        found: (u32, u32),
        need: (u32, u32),
    },
}

impl FfiError {
    pub fn mpv(lib: &MpvLibrary, code: c_int) -> Self {
        let message = unsafe {
            let ptr = (lib.mpv_error_string)(code);
            if ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        Self::Mpv { code, message }
    }
}

pub fn check(lib: &MpvLibrary, code: c_int) -> Result<(), FfiError> {
    if code < 0 {
        Err(FfiError::mpv(lib, code))
    } else {
        Ok(())
    }
}

const CANDIDATE_DLL_NAMES: &[&str] = &["mpv-2.dll", "mpv.dll", "libmpv-2.dll", "libmpv.dll"];

impl MpvLibrary {
    pub fn load(explicit: Option<&std::path::Path>) -> Result<Arc<Self>, FfiError> {
        let mut tried: Vec<String> = Vec::new();
        if let Some(path) = explicit {
            if path.is_file() {
                return unsafe { Self::load_from(path) }.map(Arc::new);
            }
            tried.push(path.display().to_string());
        }
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        let cwd = std::env::current_dir().ok();
        for base in [exe_dir, cwd].into_iter().flatten() {
            for name in CANDIDATE_DLL_NAMES {
                let candidate = base.join(name);
                if candidate.is_file() {
                    return unsafe { Self::load_from(&candidate) }.map(Arc::new);
                }
                tried.push(candidate.display().to_string());
            }
        }
        for name in CANDIDATE_DLL_NAMES {
            match unsafe { Self::load_from(std::path::Path::new(name)) } {
                Ok(lib) => return Ok(Arc::new(lib)),
                Err(e) => {
                    tracing::warn!(target: "mpv", "DLL candidate {} load failed: {}", name, e);
                    tried.push(format!("{} ({})", name, e));
                }
            }
        }
        Err(FfiError::DllNotFound(tried.join(", ")))
    }

    unsafe fn load_from(path: &std::path::Path) -> Result<Self, FfiError> {
        let lib = unsafe {
            #[cfg(windows)]
            {
                let win = libloading::os::windows::Library::load_with_flags(
                    path,
                    libloading::os::windows::LOAD_WITH_ALTERED_SEARCH_PATH,
                )
                .map_err(|source| FfiError::DllLoad {
                    path: path.display().to_string(),
                    source,
                })?;
                libloading::Library::from(win)
            }
            #[cfg(not(windows))]
            {
                Library::new(path).map_err(|source| FfiError::DllLoad {
                    path: path.display().to_string(),
                    source,
                })?
            }
        };
        unsafe fn sym<T>(lib: &Library, name: &'static str) -> Result<T, FfiError>
        where
            T: Copy,
        {
            let s: Symbol<'_, T> = unsafe {
                lib.get(name.as_bytes())
                    .map_err(|_| FfiError::MissingSymbol { symbol: name })?
            };
            Ok(*s)
        }
        let mpv_client_api_version: MpvClientApiVersionFn = unsafe { sym(&lib, "mpv_client_api_version")? };
        let raw_ver = unsafe { mpv_client_api_version() };
        let api_version = ((raw_ver >> 16) as u32, (raw_ver & 0xFFFF) as u32);
        if api_version < REQUIRED_API_VERSION {
            return Err(FfiError::ApiTooOld {
                found: api_version,
                need: REQUIRED_API_VERSION,
            });
        }
        Ok(unsafe {
            Self {
                mpv_create: sym(&lib, "mpv_create")?,
                mpv_initialize: sym(&lib, "mpv_initialize")?,
                mpv_terminate_destroy: sym(&lib, "mpv_terminate_destroy")?,
                mpv_set_option_string: sym(&lib, "mpv_set_option_string")?,
                mpv_set_property_string: sym(&lib, "mpv_set_property_string")?,
                mpv_command: sym(&lib, "mpv_command")?,
                mpv_set_property: sym(&lib, "mpv_set_property")?,
                mpv_get_property: sym(&lib, "mpv_get_property")?,
                mpv_free_node_contents: sym(&lib, "mpv_free_node_contents")?,
                mpv_observe_property: sym(&lib, "mpv_observe_property")?,
                mpv_request_log_messages: sym(&lib, "mpv_request_log_messages")?,
                mpv_wait_event: sym(&lib, "mpv_wait_event")?,
                mpv_error_string: sym(&lib, "mpv_error_string")?,
                mpv_free: sym(&lib, "mpv_free")?,
                mpv_render_context_create: sym(&lib, "mpv_render_context_create")?,
                mpv_render_context_set_update_callback: sym(
                    &lib,
                    "mpv_render_context_set_update_callback",
                )?,
                mpv_render_context_update: sym(&lib, "mpv_render_context_update")?,
                mpv_render_context_get_info: sym(&lib, "mpv_render_context_get_info")?,
                mpv_render_context_render: sym(&lib, "mpv_render_context_render")?,
                mpv_render_context_report_swap: sym(&lib, "mpv_render_context_report_swap")?,
                mpv_render_context_free: sym(&lib, "mpv_render_context_free")?,
                mpv_get_time_us: sym(&lib, "mpv_get_time_us")?,
                mpv_client_api_version,
                api_version,
                _lib: lib,
            }
        })
    }
}

pub fn to_cstring_vec(args: &[&str]) -> Vec<CString> {
    args.iter()
        .map(|s| CString::new(*s).unwrap_or_default())
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    /// ADVANCED_CONTROL 常量必须为 10（render.h:287）。
    /// 曾误用 9=WL_DISPLAY（R17），传 int* 当 wayland display 指针。
    #[test]
    fn advanced_control_constant_is_10() {
        assert_eq!(MPV_RENDER_PARAM_ADVANCED_CONTROL, 10);
    }

    /// report_swap 签名对齐 render.h:722 `void`（曾误定义 `-> i64`）。
    /// 函数指针 size_of 不区分返回类型，无法静态断言；此处用注释 +
    /// 头文件版本锚定。运行时由 dlsym 加载实际符号，签名不符会 UB。
    /// 维护契约：改此类型前先核对 vendor/mpv-dev/include/mpv/render.h。
    #[test]
    fn report_swap_signature_documented_void() {
        // 占位测试：签名正确性靠 code review + 头文件锚定保证。
        // 存在此测试确保 ffi test 模块编译 + 提醒维护者核对。
        let _ = std::mem::size_of::<MpvRenderContextReportSwapFn>();
    }
}
