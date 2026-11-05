use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::sync::PoisonError;

use crate::client::MpvClient;
use crate::ffi::*;

pub struct MpvRenderContext {
    lib: Arc<MpvLibrary>,
    ctx: *mut MpvRenderContextHandle,
    /// C 侧回调槽指针（`Arc<CallbackSlot>::into_raw`，Sized → thin）。
    update_callback: Option<NonNull<CallbackSlot>>,
}

unsafe impl Send for MpvRenderContext {}
unsafe impl Sync for MpvRenderContext {}

/// update 回调槽。
///
/// **指针形态契约**：mpv 的 `cb_ctx` 是 thin `void*`，只能指向 Sized 数据。
/// `Arc<dyn Fn>` 的 `into_raw` 是 fat pointer（data+vtable 16 字节），
/// 直接过 C 会丢 vtable——trampoline 虚调用跳垃圾地址（实测播放即
/// ACCESS_VIOLATION）。因此 C 持有 `Arc<CallbackSlot>`（Sized）的
/// into_raw 指针：
/// - trampoline 调用期 `increment_strong_count` +1，与 Rust 侧的
///   卸载（`from_raw` drop）并发时槽对象存活到调用结束；
/// - `Mutex<Option<..>>` 处理卸载后到达的回调（None = 跳过），
///   替换/移除时在锁内置 None，正在执行的回调持自己的克隆。
struct CallbackSlot {
    cb: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl CallbackSlot {
    fn lock_cb(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.cb
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `MPV_RENDER_PARAM_NEXT_FRAME_INFO` 的下一帧信息（display 配速判定用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// 有帧要显示
    pub present: bool,
    /// 只是重绘当前帧（暂停/配置变化等，非新帧）
    pub redraw: bool,
    /// display-sync 重复显示上一帧：内容与已渲染的上一帧相同，
    /// 可跳过 SW 缩放与纹理上传（`render_sw_skip`）
    pub repeat: bool,
    /// mpv 期望调用方阻塞到 vsync（block=true 时由 mpv render 内部处理）
    pub block_vsync: bool,
    /// 帧期望显示的绝对时间（微秒，与 `MpvClient::get_time_us` 同基准）。
    /// redraw 帧或 vsync locked timing 时为 0。
    pub target_time: i64,
}

impl FrameInfo {
    /// render.h：PRESENT=1、REDRAW=2、REPEAT=4、BLOCK_VSYNC=8
    fn from_info(flags: u64, target_time: i64) -> Self {
        Self {
            present: flags & 0x1 != 0,
            redraw: flags & 0x2 != 0,
            repeat: flags & 0x4 != 0,
            block_vsync: flags & 0x8 != 0,
            target_time,
        }
    }
}

/// trampoline（mpv 内部线程调用）：调用期持一个槽强引用（increment
/// 后 from_raw 接管，drop 归还），与 Rust 侧卸载并发时槽存活；槽内
/// 回调已被卸载（None）则跳过。回调在锁外以自有克隆执行。
unsafe extern "C" fn update_trampoline(cb_ctx: *mut c_void) {
    if let Some(ptr) = NonNull::new(cb_ctx as *mut CallbackSlot) {
        unsafe { Arc::increment_strong_count(ptr.as_ptr()) };
        let slot = unsafe { Arc::from_raw(ptr.as_ptr()) };
        if let Some(cb) = slot.lock_cb() {
            // extern "C" 边界的 panic 会 abort 整个进程（Rust 语义：
            // 跨 FFI unwind 是 UB，实现选择 abort）。egui 的
            // request_repaint 不 panic，此处为防御性收口；panic=abort
            // 构建下本调用无效但无害。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb()));
        }
    }
}

impl MpvRenderContext {
    /// # Safety
    /// `client` 必须来自存活的 `MpvLibrary` 且尚未创建过 render context。
    pub unsafe fn create_sw(client: &MpvClient) -> Result<Self, FfiError> {
        let lib = client.library().clone();
        let mut ctx: *mut MpvRenderContextHandle = std::ptr::null_mut();
        let api_type = MPV_RENDER_API_TYPE_SW.as_ptr() as *const std::ffi::c_char;
        // 不启 ADVANCED_CONTROL：SW 路径不需要严格 pacing 契约
        // （next_frame_info/block/skip 的前置依赖）。启用后 hwdec 切换时
        // mpv 核心等 report_swap 闭环，render_sw 失败帧不调 report_swap
        // → set_property("hwdec") 阻塞 → UI 无响应。SW 用 block=false
        // 轮询让 mpv 自管时序，report_swap 对 SW 非必要。
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_API_TYPE,
                data: api_type as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        check(&lib, unsafe {
            (lib.mpv_render_context_create)(&mut ctx, client.raw_handle(), params.as_mut_ptr())
        })?;
        Ok(Self {
            lib,
            ctx,
            update_callback: None,
        })
    }

    /// GL 互操作 render context（glow 后端用，mpv 直接渲染到 GL FBO）。
    /// # Safety
    /// `client` 必须来自存活的 `MpvLibrary` 且尚未创建过 render context。
    /// 调用线程必须当前 GL context（= eframe glow context）。
    pub unsafe fn create(
        client: &MpvClient,
        get_proc_address: unsafe extern "C" fn(*mut c_void, *const std::ffi::c_char) -> *mut c_void,
    ) -> Result<Self, FfiError> {
        let lib = client.library().clone();
        let mut ctx: *mut MpvRenderContextHandle = std::ptr::null_mut();
        let api_type = MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *const std::ffi::c_char;
        let mut init_params = MpvOpenGLInitParams {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx: std::ptr::null_mut(),
        };
        // 不启 ADVANCED_CONTROL（与 create_sw 同因，问题更严重）：glow 的
        // render 只能在该 UI 线程的 paint callback 里执行，而 UI 线程同时在
        // poll_events 里做同步 get_property（FileLoaded/Reconfig/Seek）。
        // 启用后核心在帧呈现/解码器重建处等 render 闭环，render 又要等
        // UI 线程从同步属性读里出来 → 环形死锁（实测切 hwdec / 以软解起播
        // 均无响应，FREEZE phase=ev:file-loaded）。关闭后核心不再等 render，
        // display-resample 改用 render 调用时间估算 vsync（report_swap 变
        // 无害 no-op），平滑度损失可接受。
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_API_TYPE,
                data: api_type as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut init_params as *mut MpvOpenGLInitParams as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        check(&lib, unsafe {
            (lib.mpv_render_context_create)(&mut ctx, client.raw_handle(), params.as_mut_ptr())
        })?;
        Ok(Self {
            lib,
            ctx,
            update_callback: None,
        })
    }

    pub fn set_update_callback(&mut self, callback: Box<dyn Fn() + Send + Sync>) {
        self.unload_update_callback();
        let slot: Arc<CallbackSlot> = Arc::new(CallbackSlot {
            cb: Mutex::new(Some(Arc::from(callback))),
        });
        // CallbackSlot 是 Sized：into_raw 产 thin 指针，可安全过 C void*。
        let raw = Arc::into_raw(slot);
        unsafe {
            (self.lib.mpv_render_context_set_update_callback)(
                self.ctx,
                Some(update_trampoline),
                raw as *mut c_void,
            );
        }
        self.update_callback = NonNull::new(raw as *mut CallbackSlot);
    }

    /// 卸载当前回调：mpv 先停止分发（正在执行的 trampoline 已持槽强
    /// 引用与回调克隆，计数保护），再清空槽内回调，最后释放 Rust 侧
    /// 引用——槽与回调对象的真正释放由最后一个持有者完成。
    fn unload_update_callback(&mut self) {
        unsafe {
            (self.lib.mpv_render_context_set_update_callback)(self.ctx, None, std::ptr::null_mut());
        }
        if let Some(old) = self.update_callback.take() {
            let slot = unsafe { Arc::from_raw(old.as_ptr()) };
            *slot.cb.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    pub fn request_update(&self) -> u64 {
        unsafe { (self.lib.mpv_render_context_update)(self.ctx) }
    }

    /// 查询下一帧信息（`request_update` 报告 UPDATE_FRAME 后、render 前调用）。
    /// mpv 不支持或暂无帧时返回 None。
    pub fn next_frame_info(&self) -> Option<FrameInfo> {
        let mut info = MpvRenderFrameInfo {
            flags: 0,
            target_time: 0,
        };
        let param = MpvRenderParam {
            kind: MPV_RENDER_PARAM_NEXT_FRAME_INFO,
            data: &mut info as *mut MpvRenderFrameInfo as *mut c_void,
        };
        let code = unsafe { (self.lib.mpv_render_context_get_info)(self.ctx, param) };
        (code >= 0).then(|| FrameInfo::from_info(info.flags, info.target_time))
    }

    /// 只取走帧、不做 SW 渲染（`MPV_RENDER_PARAM_SKIP_RENDERING`）：
    /// display-sync 的重复帧内容与上一帧相同，跳过 SW 缩放与纹理上传。
    /// mpv 侧照常推进时序（timing 行为与真渲染一致），返回后帧被视为已呈现。
    ///
    /// # Safety
    /// 与本 context 的其他渲染调用互斥。
    pub unsafe fn render_sw_skip(&self, block_for_target_time: bool) -> Result<(), FfiError> {
        let mut skip: std::ffi::c_int = 1;
        let mut block: std::ffi::c_int = block_for_target_time.into();
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_SKIP_RENDERING,
                data: &mut skip as *mut std::ffi::c_int as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut std::ffi::c_int as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        check(&self.lib, unsafe {
            (self.lib.mpv_render_context_render)(self.ctx, params.as_mut_ptr())
        })
    }

    /// 渲染当前帧到 rgb0 CPU buffer（top-down 行序，stride = width*4）。
    /// `buffer.len()` 必须 >= width*height*4；调用方负责把第 4 字节填成 255。
    /// `block_for_target_time`：true = 等 VO 呈现时机（flip_page 握手），
    /// 与不传该参数时 mpv 的默认行为一致；false = 立即返回。
    ///
    /// # Safety
    /// `buffer` 长度必须 >= width*height*4，且与本 context 的渲染线程互斥调用。
    pub unsafe fn render_sw(
        &self,
        width: i32,
        height: i32,
        buffer: &mut [u8],
        block_for_target_time: bool,
    ) -> Result<(), FfiError> {
        let stride: usize = width as usize * 4;
        debug_assert!(buffer.len() >= stride * height as usize);
        let mut size = [width, height];
        let format = MPV_RENDER_API_TYPE_SW_FORMAT_RGB0.as_ptr() as *const std::ffi::c_char;
        let mut stride_param: usize = stride;
        let mut block: std::ffi::c_int = block_for_target_time.into();
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_SW_FORMAT,
                data: format as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_SW_STRIDE,
                data: &mut stride_param as *mut usize as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_SW_POINTER,
                data: buffer.as_mut_ptr() as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block as *mut std::ffi::c_int as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        check(&self.lib, unsafe {
            (self.lib.mpv_render_context_render)(self.ctx, params.as_mut_ptr())
        })
    }

    /// 报告一次呈现已完成（display-resample vsync 反馈，R03）。
    /// 仅 ADVANCED_CONTROL 启用时参与帧时序闭环；当前两条路径都未启用，
    /// 调用为空操作，保留以在呈现点显式上报（对齐 render API 语义）。
    ///
    /// # Safety
    /// 与本 context 的其他渲染调用互斥。
    pub unsafe fn report_swap(&self) {
        unsafe { (self.lib.mpv_render_context_report_swap)(self.ctx) }
    }

    /// GL 互操作渲染：mpv 直接渲染到 GL FBO（glow 后端用）。
    /// `fbo` 是 GL framebuffer 对象名，`w`/`h` 是 FBO 尺寸，`flip_y` 控制上下翻转。
    /// `block`：true = 等 VO 呈现时机（flip_page 握手，mpv 默认）；false = 立即返回，
    /// 时序由调用方自负（pacing 调度用，离帧预定时间远时先画旧纹理不阻塞）。
    ///
    /// # Safety
    /// 调用线程必须当前 GL context，且与本 context 的其他渲染调用互斥。
    pub unsafe fn render(
        &self,
        fbo: i32,
        w: i32,
        h: i32,
        flip_y: bool,
        block: bool,
    ) -> Result<(), FfiError> {
        let mut fbo_param = MpvOpenGLFbo {
            fbo,
            w,
            h,
            internal_format: 0,
        };
        let mut flip: std::ffi::c_int = flip_y.into();
        let mut block_param: std::ffi::c_int = block.into();
        let mut params = [
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo_param as *mut MpvOpenGLFbo as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip as *mut std::ffi::c_int as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block_param as *mut std::ffi::c_int as *mut c_void,
            },
            MpvRenderParam {
                kind: MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        check(&self.lib, unsafe {
            (self.lib.mpv_render_context_render)(self.ctx, params.as_mut_ptr())
        })
    }
}

impl Drop for MpvRenderContext {
    fn drop(&mut self) {
        self.unload_update_callback();
        unsafe {
            (self.lib.mpv_render_context_free)(self.ctx);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_info_from_info_decodes_flags_and_target_time() {
        let info = FrameInfo::from_info(0x1 | 0x4, 123_456);
        assert!(info.present);
        assert!(!info.redraw);
        assert!(info.repeat);
        assert!(!info.block_vsync);
        assert_eq!(info.target_time, 123_456);
    }

    #[test]
    fn frame_info_from_info_redraw_and_block_vsync() {
        let info = FrameInfo::from_info(0x2 | 0x8, 0);
        assert!(!info.present);
        assert!(info.redraw);
        assert!(!info.repeat);
        assert!(info.block_vsync);
        assert_eq!(info.target_time, 0);
    }
}
