use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::events::{EndFileReason, EngineEvent, PropertyChange};
use crate::ffi::*;
use crate::node;

/// 构造 `CString`，NUL 字节时 warn 并回退空串（保持原 `unwrap_or_default`
/// 行为但补诊断，避免属性名/值含 NUL 时静默设空）。
fn cstring_checked(s: &str) -> CString {
    match CString::new(s) {
        Ok(c) => c,
        Err(_) => {
            tracing::warn!(target: "mpv", "string contains NUL byte, sent as empty: {:?}", s);
            CString::default()
        }
    }
}

/// 章节元数据（chapter-list 条目）。
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub title: String,
    pub time: f64,
}

/// 轨道元数据（track-list 条目；kind 为 "audio" / "sub" / "video"）。
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: i64,
    pub kind: String,
    pub selected: bool,
    pub title: String,
    pub lang: String,
}

pub struct MpvClient {
    lib: Arc<MpvLibrary>,
    handle: *mut MpvHandle,
    dead: AtomicBool,
}

unsafe impl Send for MpvClient {}
unsafe impl Sync for MpvClient {}

pub struct ClientOptions {
    pub hwdec: String,
    pub extra_options: Vec<(String, String)>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            hwdec: "auto-safe".into(),
            extra_options: Vec::new(),
        }
    }
}

/// `observe_property` 的属性格式（替代裸 `MPV_FORMAT_*` 常量，调用方无需背常量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsFormat {
    Double,
    Flag,
    String,
    Int64,
}

impl ObsFormat {
    pub(crate) fn as_c_int(self) -> c_int {
        match self {
            ObsFormat::Double => MPV_FORMAT_DOUBLE,
            ObsFormat::Flag => MPV_FORMAT_FLAG,
            ObsFormat::String => MPV_FORMAT_STRING,
            ObsFormat::Int64 => MPV_FORMAT_INT64,
        }
    }
}

impl MpvClient {
    pub fn create(lib: Arc<MpvLibrary>, options: &ClientOptions) -> Result<Self, FfiError> {
        unsafe {
            let handle = (lib.mpv_create)();
            if handle.is_null() {
                return Err(FfiError::Mpv {
                    code: -1,
                    message: "mpv_create returned null".into(),
                });
            }
            let client = Self {
                lib,
                handle,
                dead: AtomicBool::new(false),
            };
            client.set_option_string("vo", "libmpv")?;
            client.set_option_string("hwdec", &options.hwdec)?;
            for (name, value) in &options.extra_options {
                client.set_option_string(name, value)?;
            }
            check(&client.lib, (client.lib.mpv_initialize)(client.handle))?;
            client.request_log_messages("info");
            Ok(client)
        }
    }

    /// PoC 构造（wid 自管渲染路径，见 docs/poc-wid-gpu-render.md）：
    /// 不进 render API 模式（无 `vo=libmpv`），改设 `vo=<vo>` + `wid=<hwnd>`，
    /// mpv 自建 D3D11 swapchain 嵌入指定窗口，与 eframe/wgpu 上下文零耦合。
    /// Windows 下 wid 选项收 HWND 整数，须在 initialize 前设置。
    /// 产品路径勿用（现有渲染管线仍走 `create` + render context）。
    pub fn create_wid(
        lib: Arc<MpvLibrary>,
        hwnd: isize,
        vo: &str,
        options: &ClientOptions,
    ) -> Result<Self, FfiError> {
        unsafe {
            let handle = (lib.mpv_create)();
            if handle.is_null() {
                return Err(FfiError::Mpv {
                    code: -1,
                    message: "mpv_create returned null".into(),
                });
            }
            let client = Self {
                lib,
                handle,
                dead: AtomicBool::new(false),
            };
            client.set_option_string("vo", vo)?;
            client.set_option_string("wid", &hwnd.to_string())?;
            client.set_option_string("hwdec", &options.hwdec)?;
            for (name, value) in &options.extra_options {
                client.set_option_string(name, value)?;
            }
            check(&client.lib, (client.lib.mpv_initialize)(client.handle))?;
            client.request_log_messages("info");
            Ok(client)
        }
    }

    pub fn library(&self) -> &Arc<MpvLibrary> {
        &self.lib
    }

    /// mpv 内部时钟（微秒），与 render `target_time` 同基准，用于 SW pacing。
    pub fn get_time_us(&self) -> i64 {
        unsafe { (self.lib.mpv_get_time_us)(self.handle) }
    }

    /// render 上下文创建需要原始句柄（crate 内部）。
    pub(crate) fn raw_handle(&self) -> *mut MpvHandle {
        self.handle
    }

    /// 初始化期选项设置（vo/hwdec 等；crate 内部）。
    /// `mpv_set_option_string` 按契约只在 `mpv_initialize` 之前调用。
    pub(crate) fn set_option_string(&self, name: &str, value: &str) -> Result<(), FfiError> {
        unsafe {
            let name = cstring_checked(name);
            let value = cstring_checked(value);
            check(
                &self.lib,
                (self.lib.mpv_set_option_string)(self.handle, name.as_ptr(), value.as_ptr()),
            )
        }
    }

    /// 订阅 mpv 内部日志（事件线程转发到 tracing，target = "mpv"）。
    /// 解码器/硬件初始化失败等只出现在这些日志里，排查必看。
    fn request_log_messages(&self, min_level: &str) {
        let min_level = &std::env::var("DYPLAYER_MPV_LOG_LEVEL").unwrap_or_else(|_| min_level.into());
        let level = cstring_checked(min_level.as_str());
        unsafe {
            let code = (self.lib.mpv_request_log_messages)(self.handle, level.as_ptr());
            if code < 0 {
                tracing::warn!(target: "mpv", "mpv_request_log_messages failed: {}", code);
            }
        }
    }

    /// 运行时属性写入（走 `mpv_set_property_string`，初始化后修改属性的正确 API）。
    pub fn set_property_string(&self, name: &str, value: &str) -> Result<(), FfiError> {
        unsafe {
            let name = cstring_checked(name);
            let value = cstring_checked(value);
            check(
                &self.lib,
                (self.lib.mpv_set_property_string)(self.handle, name.as_ptr(), value.as_ptr()),
            )
        }
    }

    fn set_property_raw(&self, name: &str, fmt: c_int, val: *mut c_void) -> Result<(), FfiError> {
        unsafe {
            let cname = cstring_checked(name);
            check(
                &self.lib,
                (self.lib.mpv_set_property)(self.handle, cname.as_ptr(), fmt, val),
            )
        }
    }

    pub fn set_property_i64(&self, name: &str, value: i64) -> Result<(), FfiError> {
        self.set_property_raw(name, MPV_FORMAT_INT64, &value as *const i64 as *mut c_void)
    }


    pub fn sub_add(&self, path: &str, select: bool, title: &str) -> Result<(), FfiError> {
        let flags = if select { "select" } else { "auto" };
        self.command(&["sub-add", path, flags, title])
    }

    pub fn command(&self, args: &[&str]) -> Result<(), FfiError> {
        unsafe {
            let cstrs = to_cstring_vec(args);
            let mut ptrs: Vec<*const std::ffi::c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            check(
                &self.lib,
                (self.lib.mpv_command)(self.handle, ptrs.as_ptr()),
            )
        }
    }

    fn get_property_raw(&self, name: &str, fmt: c_int, out: *mut c_void) -> Result<(), FfiError> {
        unsafe {
            let cname = cstring_checked(name);
            check(
                &self.lib,
                (self.lib.mpv_get_property)(self.handle, cname.as_ptr(), fmt, out),
            )
        }
    }

    pub fn get_property_f64(&self, name: &str) -> Result<f64, FfiError> {
        let mut value: f64 = 0.0;
        self.get_property_raw(name, MPV_FORMAT_DOUBLE, &mut value as *mut f64 as *mut c_void)?;
        Ok(value)
    }

    pub fn get_property_i64(&self, name: &str) -> Result<i64, FfiError> {
        let mut value: i64 = 0;
        self.get_property_raw(name, MPV_FORMAT_INT64, &mut value as *mut i64 as *mut c_void)?;
        Ok(value)
    }

    /// 读取字符串属性（如 `hwdec-current`、`video-format`）。
    /// 返回的内存由 mpv 分配，由 [`node::MpvStrGuard`] Drop 释放（panic-safe）。
    pub fn get_property_string(&self, name: &str) -> Result<String, FfiError> {
        let mut ptr: *mut c_char = std::ptr::null_mut();
        self.get_property_raw(
            name,
            MPV_FORMAT_STRING,
            &mut ptr as *mut *mut c_char as *mut c_void,
        )?;
        let guard = unsafe { node::MpvStrGuard::from_ptr(ptr, self.lib.clone()) };
        if guard.is_null() {
            return Ok(String::new());
        }
        Ok(unsafe { guard.to_string() })
    }

    pub fn get_property_flag(&self, name: &str) -> Result<bool, FfiError> {
        let mut value: c_int = 0;
        self.get_property_raw(name, MPV_FORMAT_FLAG, &mut value as *mut c_int as *mut c_void)?;
        Ok(value != 0)
    }

    pub fn observe_property(
        &self,
        userdata: u64,
        name: &str,
        format: ObsFormat,
    ) -> Result<(), FfiError> {
        unsafe {
            let cname = cstring_checked(name);
            check(
                &self.lib,
                (self.lib.mpv_observe_property)(
                    self.handle,
                    userdata,
                    cname.as_ptr(),
                    format.as_c_int(),
                ),
            )
        }
    }

    pub fn load_file(&self, path: &str) -> Result<(), FfiError> {
        self.command(&["loadfile", path, "replace"])
    }

    /// 读取复合属性为 owned [`node::Node`]；释放由 [`node::NodeGuard`] Drop 负责。
    fn get_node(&self, name: &str) -> Result<node::Node, FfiError> {
        let mut guard = node::NodeGuard::zeroed(self.lib.clone());
        unsafe {
            let cname = cstring_checked(name);
            check(
                &self.lib,
                (self.lib.mpv_get_property)(
                    self.handle,
                    cname.as_ptr(),
                    MPV_FORMAT_NODE,
                    guard.node_ptr() as *mut c_void,
                ),
            )?;
            Ok(guard.read())
        }
    }

    /// 读取轨道列表（track-list 节点数组：id/type/selected/title/lang）。
    /// 结构不符或为空时返回空列表，不视为错误。
    pub fn get_track_list(&self) -> Result<Vec<Track>, FfiError> {
        let node = self.get_node("track-list")?;
        Ok(node::tracks_from_node(&node))
    }

    /// 读取章节列表（chapter-list 节点数组：title 字符串 + time 秒）。
    /// 无章节的媒体返回空列表。
    pub fn get_chapter_list(&self) -> Result<Vec<Chapter>, FfiError> {
        let node = self.get_node("chapter-list")?;
        Ok(node::chapters_from_node(&node))
    }

    fn next_event(&self, timeout: f64) -> Option<EngineEvent> {
        unsafe {
            let ev = (self.lib.mpv_wait_event)(self.handle, timeout);
            if ev.is_null() {
                self.dead.store(true, Ordering::SeqCst);
                return Some(EngineEvent::Shutdown);
            }
            let id = (*ev).event_id;
            match id {
                MPV_EVENT_NONE => None,
                MPV_EVENT_SHUTDOWN => {
                    self.dead.store(true, Ordering::SeqCst);
                    Some(EngineEvent::Shutdown)
                }
                MPV_EVENT_START_FILE => {
                    let entry_id = if (*ev).data.is_null() {
                        0
                    } else {
                        *((*ev).data as *const i64)
                    };
                    Some(EngineEvent::StartFile { entry_id })
                }
                MPV_EVENT_END_FILE => {
                    if (*ev).data.is_null() {
                        return Some(EngineEvent::EndFile {
                            reason: EndFileReason::Unknown,
                            error: None,
                        });
                    }
                    let ef = &*((*ev).data as *const MpvEventEndFile);
                    Some(EngineEvent::EndFile {
                        reason: EndFileReason::from_raw(ef.reason),
                        error: if ef.error < 0 {
                            Some(format!("mpv error {}", ef.error))
                        } else {
                            None
                        },
                    })
                }
                MPV_EVENT_FILE_LOADED => Some(EngineEvent::FileLoaded),
                MPV_EVENT_LOG_MESSAGE => {
                    if (*ev).data.is_null() {
                        return None;
                    }
                    let m = &*((*ev).data as *const MpvEventLogMessage);
                    let text = |p: *const c_char| {
                        if p.is_null() {
                            String::new()
                        } else {
                            CStr::from_ptr(p).to_string_lossy().into_owned()
                        }
                    };
                    let level = text(m.level);
                    let line = format!("[{}] {}: {}", level, text(m.prefix), text(m.text));
                    match level.as_str() {
                        "error" | "fatal" => tracing::error!(target: "mpv", "{}", line),
                        "warn" => tracing::warn!(target: "mpv", "{}", line),
                        _ => tracing::debug!(target: "mpv", "{}", line),
                    }
                    None
                }
                MPV_EVENT_VIDEO_RECONFIG => Some(EngineEvent::VideoReconfig),
                MPV_EVENT_SEEK => Some(EngineEvent::Seek),
                MPV_EVENT_PLAYBACK_RESTART => Some(EngineEvent::PlaybackRestart),
                MPV_EVENT_PROPERTY_CHANGE => {
                    if (*ev).data.is_null() {
                        return None;
                    }
                    let prop = &*((*ev).data as *const MpvEventProperty);
                    let name = if prop.name.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(prop.name).to_string_lossy().into_owned()
                    };
                    let change = match (name.as_str(), prop.format) {
                        ("time-pos", MPV_FORMAT_DOUBLE) => {
                            let v = *(prop.data as *const f64);
                            PropertyChange::TimePos(v)
                        }
                        ("duration", MPV_FORMAT_DOUBLE) => {
                            let v = *(prop.data as *const f64);
                            PropertyChange::Duration(v)
                        }
                        ("speed", MPV_FORMAT_DOUBLE) => {
                            let v = *(prop.data as *const f64);
                            PropertyChange::Speed(v)
                        }
                        ("pause", MPV_FORMAT_FLAG) => {
                            let v = *(prop.data as *const c_int);
                            PropertyChange::Paused(v != 0)
                        }
                        ("demuxer-cache-time", MPV_FORMAT_DOUBLE) => {
                            let v = *(prop.data as *const f64);
                            PropertyChange::DemuxerCacheTime(v)
                        }
                        ("paused-for-cache", MPV_FORMAT_FLAG) => {
                            let v = *(prop.data as *const c_int);
                            PropertyChange::PausedForCache(v != 0)
                        }
                        ("core-idle", MPV_FORMAT_FLAG) => {
                            let v = *(prop.data as *const c_int);
                            PropertyChange::CoreIdle(v != 0)
                        }
                        _ => PropertyChange::Other { name },
                    };
                    Some(EngineEvent::PropertyChange(change))
                }
                _ => None,
            }
        }
    }

    /// 事件线程退出判定（crate 内部）。
    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub fn spawn_event_loop(
        self: &Arc<Self>,
        tx: mpsc::Sender<EngineEvent>,
        on_property: impl Fn(&PropertyChange) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let client = Arc::clone(self);
        std::thread::Builder::new()
            .name("mpv-events".into())
            .spawn(move || {
                loop {
                    match client.next_event(0.25) {
                        Some(EngineEvent::Shutdown) => {
                            let _ = tx.send(EngineEvent::Shutdown);
                            break;
                        }
                        Some(EngineEvent::PropertyChange(change)) => {
                            on_property(&change);
                            if tx.send(EngineEvent::PropertyChange(change)).is_err() {
                                break;
                            }
                        }
                        Some(event) => {
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                        None => {
                            if client.is_dead() {
                                break;
                            }
                        }
                    }
                }
            })
            .expect("failed to spawn mpv event loop thread")
    }
}

impl Drop for MpvClient {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_null() {
                (self.lib.mpv_terminate_destroy)(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NUL 字节时回退空串，不 panic（F5）。
    #[test]
    fn cstring_checked_nul_falls_back_to_empty() {
        let c = cstring_checked("ok\0bad");
        assert_eq!(c.to_bytes(), b"");
        let c = cstring_checked("normal");
        assert_eq!(c.to_bytes(), b"normal");
    }
}

