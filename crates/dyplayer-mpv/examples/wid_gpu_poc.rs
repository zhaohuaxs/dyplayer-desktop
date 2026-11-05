//! PoC：libmpv `wid` 自管渲染（vo=gpu-next）8K 播放验证（渲染与呈现全由解码端自管）。
//! 方案与判据见 dyplayer/docs/poc-wid-gpu-render.md。
//!
//! 结构：Win32 自建窗口（黑底，540x960）→ `MpvClient::create_wid`（vo=gpu-next
//! + wid）→ loadfile → PeekMessage 循环里每 2s 采样（进程 CPU 核数 / time-pos /
//!   vo-drop-frame-count / estimated-vf-fps）→ 30s 后退出并汇总。
//!
//! 用法：
//! cargo run -p dyplayer-mpv --example wid_gpu_poc -- [video_path] [vo_mode] [--fs] [--secs N] [--overlay]
//! `--fs`/`--fullscreen`：WS_POPUP 无边框铺满主屏（视觉确认）；`--secs N`：运行秒数（默认 30）。
//! `--overlay`：PoC-2（docs/m2-overlay-danmaku.md §3）——spawn 线程建 winit 透明窗口 +
//! wgpu `PreMultiplied` surface，半透明品红动画矩形叠在播放窗口上，验证 DComp 真混合。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use dyplayer_mpv::{ClientOptions, MpvClient, MpvLibrary};

const TITLE: &str = "DyPlayer wid GPU PoC";
const CLASS: &str = "DYPWIDPOC";
const RUN_SECS: u64 = 30;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_VIDEO: &str = r"D:\zhxs\Videos\[8K] 220515 战士레이싱모데…ᅥᆯ화 YoonSeolHwa LOL 잔나 코.mkv";

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    lo: u32,
    hi: u32,
}

impl FileTime {
    fn secs(self) -> f64 {
        ((self.lo as u64) | ((self.hi as u64) << 32)) as f64 * 1e-7
    }
}

#[repr(C)]
struct Msg {
    hwnd: usize,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

#[repr(C)]
struct WndClassW {
    style: u32,
    proc: usize,
    cls_extra: i32,
    wnd_extra: i32,
    inst: usize,
    icon: usize,
    cursor: usize,
    background: usize,
    menu_name: *const u16,
    class_name: *const u16,
}

unsafe extern "system" {
    fn GetModuleHandleW(lp: *const u16) -> usize;
    fn RegisterClassW(wc: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32,
        cls: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: usize,
        menu: usize,
        inst: usize,
        param: *mut core::ffi::c_void,
    ) -> usize;
    fn DefWindowProcW(hwnd: usize, msg: u32, wp: usize, lp: isize) -> isize;
    fn PeekMessageW(msg: *mut Msg, hwnd: usize, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn GetStockObject(idx: i32) -> usize;
    fn GetSystemMetrics(idx: i32) -> i32;
    fn GetCurrentProcess() -> usize;
    fn GetProcessTimes(
        proc: usize,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
}

const WM_DESTROY: u32 = 0x0002;
const PM_REMOVE: u32 = 1;
const BLACK_BRUSH: i32 = 4;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_POPUP: u32 = 0x8000_0000;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RECT_ {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

unsafe extern "system" {
    fn GetWindowRect(hwnd: usize, rect: *mut RECT_) -> i32;
}

unsafe fn get_window_rect(hwnd: usize, rect: &mut RECT_) {
    unsafe { GetWindowRect(hwnd, rect) };
}

unsafe extern "system" fn wnd_proc(hwnd: usize, msg: u32, wp: usize, lp: isize) -> isize {
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        0
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn proc_time_secs() -> f64 {
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    unsafe {
        if GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return f64::NAN;
        }
    }
    kernel.secs() + user.secs()
}

fn main() -> anyhow::Result<()> {
    // 参数：[video_path] [vo_mode] [--fs|--fullscreen] [--secs N] [--overlay]
    let mut fullscreen = false;
    let mut run_secs = RUN_SECS;
    let mut overlay = false;
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fs" | "--fullscreen" => fullscreen = true,
            "--overlay" => overlay = true,
            "--secs" => {
                if let Some(v) = args.next() {
                    run_secs = v.parse().unwrap_or(RUN_SECS);
                }
            }
            _ => positional.push(arg),
        }
    }
    let video = positional
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_VIDEO.into());
    let vo_mode = positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| "gpu-next".into());
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let dll = root.join("dyplayer/vendor/mpv-dev/libmpv-2.dll");
    anyhow::ensure!(dll.exists(), "dll 不存在: {dll:?}");
    anyhow::ensure!(
        PathBuf::from(&video).exists(),
        "测试视频不存在: {video:?}"
    );

    let class_name = wide(CLASS);
    let title = wide(TITLE);
    let inst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WndClassW {
        style: 0,
        proc: wnd_proc as *const () as usize,
        cls_extra: 0,
        wnd_extra: 0,
        inst,
        icon: 0,
        cursor: 0,
        background: unsafe { GetStockObject(BLACK_BRUSH) },
        menu_name: std::ptr::null(),
        class_name: class_name.as_ptr(),
    };
    let atom = unsafe { RegisterClassW(&wc) };
    anyhow::ensure!(atom != 0, "RegisterClassW 失败");
    // 全屏 = WS_POPUP 无边框铺满主屏（视觉确认用）；窗口化 = 540x960 带边框。
    let (style, x, y, w, h) = if fullscreen {
        let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        (WS_POPUP, 0, 0, sw, sh)
    } else {
        (
            WS_OVERLAPPEDWINDOW,
            0x8000_0000u32 as i32,
            0x8000_0000u32 as i32, // CW_USEDEFAULT x2
            540,
            960,
        )
    };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            w,
            h,
            0,
            0,
            inst,
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(hwnd != 0, "CreateWindowExW 失败");
    unsafe { ShowWindow_compat(hwnd) };

    // PoC-2：透明覆盖窗口（品红矩形叠视频，验证 PreMultiplied 混合）。
    if overlay {
        let (ox, oy, ow, oh) = if fullscreen {
            let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
            let sh = unsafe { GetSystemMetrics(SM_CYSCREEN) };
            (0, 0, sw, sh)
        } else {
            let mut r = RECT_ { l: 0, t: 0, r: 540, b: 960 };
            unsafe { get_window_rect(hwnd, &mut r) };
            (r.l, r.t, r.r - r.l, r.b - r.t)
        };
        let handle = std::thread::Builder::new()
            .name("overlay-poc".into())
            .spawn(move || {
                if let Err(e) = overlay_window_run(ox, oy, ow, oh) {
                    println!("== OVERLAY EXIT: {e} ==");
                }
            })
            .expect("spawn overlay thread");
        // 进程退出强杀覆盖线程；不 join。
        std::mem::forget(handle);
        println!("== overlay 线程已启动（品红矩形应半透明叠在视频上）==");
    }

    println!(
        "== wid GPU PoC: vo={vo_mode} fullscreen={fullscreen} run={run_secs}s video={video} hwnd=0x{hwnd:X} =="
    );

    let lib = MpvLibrary::load(Some(&dll))?;
    let options = ClientOptions {
        hwdec: "auto-safe".into(),
        extra_options: vec![
            ("mute".into(), "yes".into()),
            ("video-sync".into(), "audio".into()),
        ],
    };
    let client = MpvClient::create_wid(lib, hwnd as isize, &vo_mode, &options)?;
    client.load_file(&video)?;
    println!("== loadfile 已下发，进入采样循环（{run_secs}s）==");

    let start = Instant::now();
    let mut last_sample = start;
    let mut last_proc = proc_time_secs();
    let mut cpu_samples: Vec<f64> = Vec::new();
    let mut quit = false;
    let mut msg = Msg {
        hwnd: 0,
        message: 0,
        wparam: 0,
        lparam: 0,
        time: 0,
        pt_x: 0,
        pt_y: 0,
    };

    loop {
        unsafe {
            while PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_DESTROY {
                    // PostQuitMessage 已在 wnd_proc 内触发；循环靠时间兜底退出
                }
                if msg.message == 0x0012 {
                    // WM_QUIT
                    quit = true;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if quit {
            println!("== 收到 WM_QUIT，提前退出 ==");
            break;
        }

        let now = Instant::now();
        if now.duration_since(last_sample) >= SAMPLE_INTERVAL {
            let dt = now.duration_since(last_sample).as_secs_f64();
            let cur_proc = proc_time_secs();
            let cores = (cur_proc - last_proc) / dt;
            last_proc = cur_proc;
            last_sample = now;
            cpu_samples.push(cores);
            let t = client.get_property_f64("time-pos").unwrap_or(-1.0);
            let drops = client.get_property_i64("vo-drop-frame-count").unwrap_or(-1);
            let vf_fps = client.get_property_f64("estimated-vf-fps").unwrap_or(0.0);
            println!(
                "t={:5.1}s cpu={:5.2}cores time-pos={:7.2}s vo-drops={drops:6} vf-fps={vf_fps:6.2}",
                now.duration_since(start).as_secs_f64(),
                cores,
                t
            );
        }
        if start.elapsed() >= Duration::from_secs(run_secs) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let elapsed = start.elapsed().as_secs_f64();
    let t_final = client.get_property_f64("time-pos").unwrap_or(-1.0);
    let drops_total = client.get_property_i64("vo-drop-frame-count").unwrap_or(-1);
    let rate = if elapsed > 0.0 && t_final > 0.0 {
        t_final / elapsed
    } else {
        0.0
    };
    let cpu_avg = if cpu_samples.is_empty() {
        f64::NAN
    } else {
        cpu_samples.iter().sum::<f64>() / cpu_samples.len() as f64
    };
    println!(
        "== RESULT: cpu_avg={cpu_avg:.2}cores playback_rate={rate:.3}x time-pos-final={t_final:.1}s vo-drops-total={drops_total} =="
    );
    let _ = client.command(&["quit"]);
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}

#[allow(non_snake_case)]
unsafe fn ShowWindow_compat(hwnd: usize) {
    // SW_SHOWNA（不抢焦点）= 8：PoC 窗口不该偷用户焦点
    unsafe extern "system" {
        fn ShowWindow(hwnd: usize, cmd: i32) -> i32;
    }
    unsafe { ShowWindow(hwnd, 8) };
}
// ---------------------------------------------------------------------------
// PoC-2：透明覆盖窗口（winit + wgpu PreMultiplied，docs/m2-overlay-danmaku.md §3）。
// 独立线程事件循环（ApplicationHandler），半透明品红动画矩形叠 mpv wid 视频之上。
mod overlay_poc {
    use std::time::{Duration, Instant};
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::window::{Window, WindowId, WindowLevel};

    const SHADER: &str = r#"
@vertex
fn vs(@location(0) p: vec2f) -> @builtin(position) vec4f {
    return vec4f(p, 0.0, 1.0);
}

@fragment
fn fs() -> @location(0) vec4f {
    // 品红 45% 预乘（PreMultiplied：rgb 已乘 alpha）
    let a = 0.45;
    return vec4f(a, 0.0, a, a);
}
"#;

    /// 覆盖窗口矩形顶点（NDC，6 顶点两三角）；y 向下为正。
    fn rect_vertices(x0: f32, y0: f32, x1: f32, y1: f32) -> [f32; 12] {
        [
            x0, y0, x1, y0, x0, y1, // tri1
            x1, y0, x1, y1, x0, y1, // tri2
        ]
    }

    fn pod_bytes(v: &[f32; 12]) -> &[u8] {
        // f32 数组无 padding，字节级重解释安全（例内避免 bytemuck 依赖）
        unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 4) }
    }

    struct Inner {
        window: Window,
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
        pipeline: wgpu::RenderPipeline,
        vbuf: wgpu::Buffer,
        start: Instant,
        last_alive: Instant,
    }

    impl Inner {
        fn render(&mut self) {
            let t = self.start.elapsed().as_secs_f32();
            // 动画矩形：中央带宽 0.5，宽 0.3，水平往复。
            let cx = (t * 0.6).sin() * 0.32;
            let v = rect_vertices(cx - 0.15, -0.25, cx + 0.15, 0.25);
            self.queue.write_buffer(&self.vbuf, 0, pod_bytes(&v));

            let frame = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => f,
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    self.surface.configure(&self.device, &self.config);
                    return;
                }
                _ => return, // Timeout/Occluded/Validation：跳帧
            };
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut enc = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("overlay-poc-enc"),
                });
            {
                let mut rpass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("overlay-poc-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // PreMultiplied 下 alpha=0 clear = 全透明，透出下层视频
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 0.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                rpass.set_pipeline(&self.pipeline);
                rpass.set_vertex_buffer(0, self.vbuf.slice(..));
                rpass.draw(0..6, 0..1);
            }
            self.queue.submit(Some(enc.finish()));
            self.queue.present(frame);

            if self.last_alive.elapsed() >= Duration::from_secs(5) {
                self.last_alive = Instant::now();
                println!("== OVERLAY alive t={t:.1}s ==");
            }
        }
    }

    struct OverlayApp {
        geom: (i32, i32, i32, i32),
        display: Option<winit::event_loop::OwnedDisplayHandle>,
        inner: Option<Inner>,
    }

    impl OverlayApp {
        /// 首次 resumed：建窗口 + wgpu 资源（winit 0.30 正统：窗口须在
        /// ActiveEventLoop 上创建，EventLoop::create_window 已 deprecated）。
        fn init(&mut self, event_loop: &ActiveEventLoop) {
            println!("== OVERLAY step: resumed, creating window ==");
            let (x, y, w, h) = self.geom;
            let attrs = Window::default_attributes()
                .with_title("DyPlayer overlay poc")
                .with_transparent(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_position(winit::dpi::PhysicalPosition::new(x, y))
                .with_inner_size(winit::dpi::PhysicalSize::new(w as u32, h as u32));
            let window = match event_loop.create_window(attrs) {
                Ok(w) => w,
                Err(e) => {
                    println!("== OVERLAY create_window 失败: {e} ==");
                    return;
                }
            };
            let inner = match init_gpu(window, w, h, self.display.take()) {
                Ok(i) => i,
                Err(e) => {
                    println!("== OVERLAY GPU 初始化失败: {e:?} ==");
                    return;
                }
            };
            println!("== OVERLAY step: gpu ready ==");
            self.inner = Some(inner);
        }
    }

    fn init_gpu(
        window: Window,
        w: i32,
        h: i32,
        display: Option<winit::event_loop::OwnedDisplayHandle>,
    ) -> anyhow::Result<Inner> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            // 透明窗口必须走 DComp visual swapchain（DxgiFromHwnd 在
            // WS_EX_NOREDIRECTIONBITMAP 窗口上 configure 会挂死）。
            backend_options: wgpu::BackendOptions {
                dx12: wgpu::Dx12BackendOptions {
                    presentation_system: wgpu::Dx12SwapchainKind::DxgiFromVisual,
                    ..Default::default()
                },
                ..Default::default()
            },
            // `as _`：目标 trait（wgpu-types 的 WgpuHasDisplayHandle）不经 facade
            // 导出无法命名，由字段类型推断（blanket impl 自动满足）。
            display: display.map(|d| Box::new(d) as _),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        println!("== OVERLAY step: instance ok ==");
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(&window)?)?
        };
        println!("== OVERLAY step: surface ok ==");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))?;
        println!("== OVERLAY step: adapter ok ==");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("overlay-poc"),
            ..Default::default()
        }))?;
        println!("== OVERLAY step: device ok ==");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow::anyhow!("surface 无可用格式"))?;
        let alpha_mode = if caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            anyhow::bail!(
                "surface 不支持 PreMultiplied（alpha_modes={:?}）",
                caps.alpha_modes
            );
        };
        println!(
            "== OVERLAY: format={format:?} alpha_mode={alpha_mode:?} size={w}x{h} =="
        );

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w as u32,
            height: h as u32,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: Vec::new(),
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay-poc-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay-poc-pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 8,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: 0,
                    }],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay-poc-vbuf"),
            size: 12 * 4,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Inner {
            window,
            device,
            queue,
            surface,
            config,
            pipeline,
            vbuf,
            start: Instant::now(),
            last_alive: Instant::now(),
        })
    }

    impl ApplicationHandler for OverlayApp {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.inner.is_none() {
                self.init(event_loop);
                if let Some(inner) = &self.inner {
                    inner.window.request_redraw();
                }
            }
        }
        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WindowId,
            event: WindowEvent,
        ) {
            let Some(inner) = &mut self.inner else {
                return;
            };
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::RedrawRequested => {
                    inner.render();
                    // 60fps 自续
                    inner.window.request_redraw();
                }
                WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                    inner.config.width = size.width;
                    inner.config.height = size.height;
                    inner.surface.configure(&inner.device, &inner.config);
                }
                _ => {}
            }
        }
    }

    pub fn run(x: i32, y: i32, w: i32, h: i32) -> anyhow::Result<()> {
        // 覆盖层跑在子线程：winit Windows 默认禁止非主线程 EventLoop，
        // 显式 opt-in（EventLoopBuilderExtWindows::any_thread）。
        use winit::platform::windows::EventLoopBuilderExtWindows;

        println!("== OVERLAY step: building event loop ==");
        let event_loop = EventLoop::builder()
            .with_any_thread(true)
            .build()?;
        println!("== OVERLAY step: event loop built ==");
        // wgpu 30 create_surface 需要 display handle（eframe 同款：
        // wgpu_integration.rs owned_display_handle 注入 InstanceDescriptor）。
        let display = event_loop.owned_display_handle();
        let mut app = OverlayApp {
            geom: (x, y, w, h),
            display: Some(display),
            inner: None,
        };
        println!("== OVERLAY step: entering run_app ==");
        event_loop.run_app(&mut app)?;
        Ok(())
    }
}

fn overlay_window_run(x: i32, y: i32, w: i32, h: i32) -> anyhow::Result<()> {
    overlay_poc::run(x, y, w, h)
}
