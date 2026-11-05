//! WGL proc address 解析（mpv GL 互操作用，glow 后端 only）。

use std::ffi::{CStr, c_char, c_void};
use std::ptr;

use windows::Win32::Graphics::OpenGL::wglGetProcAddress;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::core::PCSTR;

const OPENGL32: &[u8] = b"opengl32.dll\0";

pub unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    unsafe {
        let name_c = CStr::from_ptr(name);
        let pc = PCSTR(name_c.as_ptr() as *const u8);
        if let Some(proc) = wglGetProcAddress(pc) {
            return proc as *mut c_void;
        }
        if let Ok(module) = GetModuleHandleA(PCSTR(OPENGL32.as_ptr()))
            && let Some(proc) = GetProcAddress(module, pc)
        {
            return proc as *mut c_void;
        }
        ptr::null_mut()
    }
}