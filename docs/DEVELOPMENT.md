# DyPlayer 开发者文档

> 面向贡献者/维护者的技术文档。用户向内容见仓库根 [`README.md`](../README.md)。

## 目录

- [架构](#架构)
- [关键设计决策](#关键设计决策)
- [渲染管线与三路径现状](#渲染管线与三路径现状)
- [编译与运行](#编译与运行)
- [Windows 分发依赖与启动失败排查](#windows-分发依赖与启动失败排查)
- [发布体积与裁剪分析](#发布体积与裁剪分析)
- [代码签名](#代码签名)
- [开发资源索引](#开发资源索引)

---

## 架构

```
┌─────────────────────────────────────────────────────┐
│                  dyplayer-app (egui)                 │
│   Browse 屏（源/目录浏览） ⇄ Player 屏（播放控制）      │
│   player_app / browser / danmaku_ui / subtitles      │
└──┬──────────┬───────────────┬──────────────┬────────┘
   │          │               │              │
   ▼          ▼               ▼              ▼
 dyplayer-  dyplayer-    dyplayer-      dyplayer-
 vfs        mpv          danmaku        store
 目录/自然  libmpv FFI   DFM+ 布局 +    SQLite +
 排序抽象   render 回调  XML 规范化 +    config.toml +
            事件泵/时钟   glyph atlas    keyring 凭证
   │          │          自绘            │
   ▼          ▼               ▼          ▼
  文件系统   libmpv.dll    epaint Mesh   %LOCALAPPDATA%
            (动态加载)    (与视频同帧     %APPDATA%
                           叠加合成)     \dyplayer
```

### Workspace 组成

| Crate | 职责 |
|---|---|
| `dyplayer-vfs` | 文件源抽象（本地/WebDAV/SMB 统一）、视频/字幕/弹幕文件识别、自然排序 |
| `dyplayer-proxy` | 本地 HTTP Range 流代理：把远程 VFS 文件转成 mpv 可播的 URL（tiny_http，远程流转发） |
| `dyplayer-mpv` | libmpv render API 内嵌引擎：libloading 动态加载、FFI 封装、事件泵（`EngineEvent` 含 `VideoReconfig` 等映射）、渲染回调、播放时钟（MediaClock 软校准） |
| `dyplayer-danmaku` | DFM+ 弹幕布局核心（vendored 上游实现，已偏离 liberika）+ B 站 XML 规范化 + session/generation 管理 + ab_glyph glyph atlas + epaint 自绘渲染 |
| `dyplayer-store` | 持久化：SQLite 媒体库（源/播放历史/媒体索引/弹幕绑定）、config.toml 配置、keyring 凭证存储 |
| `dyplayer-app` | eframe/egui 应用壳：Browse/Player 双屏、GL 加载、字幕转码缓存、弹幕 UI、OSC 覆盖层、wid GPU 宿主 |

### 数据目录

| 路径 | 内容 |
|---|---|
| `%APPDATA%\dyplayer\config.toml` | 应用配置（引擎、播放行为、弹幕设置等） |
| `%LOCALAPPDATA%\dyplayer\library.db` | SQLite 媒体库（源、播放历史、媒体索引、弹幕绑定） |
| `%LOCALAPPDATA%\dyplayer\cache\subs\` | GBK 字幕转码缓存 |
| `%LOCALAPPDATA%\dyplayer\logs\` | release 日志（按天轮转，保留 7 天） |

日志经 `tracing` 输出，可用 `RUST_LOG` 环境变量控制级别（如 `RUST_LOG=dyplayer=debug`）。

---

## 关键设计决策

- **libmpv 动态加载**：编译期不链接 mpv，运行时经 `libloading` 按候选名（`mpv-2.dll`/`mpv.dll`/`libmpv-2.dll`/`libmpv.dll`）搜索；播放路径必须 canonicalize（mpv 对相对路径返回 error -13）
- **事件驱动 UI**：视频尺寸以 `MPV_EVENT_VIDEO_RECONFIG` 为权威事件持续同步，不信任 `FileLoaded` 时刻的单点快照
- **纹理 realloc 后清屏**：切集时纹理重新分配后主动清屏，避免新帧到达前采样到未定义内存产生撕裂噪点
- **自管无边框全屏**：不使用 winit `ViewportCommand::Fullscreen`，`fs_patch.rs` 一步切 THICKFRAME 样式 + 子类化拦截 `WM_NCCALCSIZE`/`WM_NCHITTEST`，规避冻结样式中间态并禁用边缘拉拽
- **MediaClock 唯一时钟源**：弹幕只收 `media_time: f64`，`calibrate_smooth` 软校准（死区 50ms / 收敛区 50–250ms / ≥250ms 硬锚），勿回退无阈值硬锚定
- **远程源全同步架构**：ureq3 + tiny_http proxy 无损转发，UI 用 `request_repaint` 轮询，不引入 async runtime
- **渲染解耦红线**：mpv 不得与 eframe 窗口 GPU 上下文耦合（禁 GL 互操作）；wid 自管 D3D11 解耦路径已实施

---

## 渲染管线与三路径现状

当前有三条渲染路径（编译期/运行时配置），**wid GPU 路径已实施**：

| 路径 | 配置 | 呈现 | 适用 |
|---|---|---|---|
| `gpu` (wid) | `engine.render_path="gpu"`（wgpu 构建） | mpv 自管 D3D11 swapchain，零拷贝 | 8K AV1 / 高码率，CPU 全留解码 |
| `sw` (默认) | `engine.render_path="sw"` | rgb0 CPU buffer → 按需上传 egui 纹理 | 兼容性保底 |
| `glow` (legacy) | `--features backend-glow` 编译期 | GL 互操作 FBO 零拷贝 | 老设备 (Win7+/DX9) |

**SW 路径细节**：eframe wgpu 后端（D3D12 + DirectComposition 呈现）+ mpv SW 软渲染（`MPV_RENDER_API_TYPE_SW`：mpv 渲到 rgb0 CPU buffer → 按需上传 egui 纹理）→ 与 UI/弹幕同帧合成。弹幕不走 mpv 的 ASS 字幕轨，而是 `dyplayer-danmaku` 在 epaint 侧自绘，与视频帧共用同一渲染上下文。

> 历史：曾用 glow (WGL) 后端 + GL FBO 共享上下文方案，在部分 Windows 机器上全屏切换后触发呈现层冻结（`SwapBuffers` 成功但帧不进 DWM 重定向 surface，不可逆）。

### 渲染方案定位（历史决策记录）

当前 **wgpu + SW 软渲染** 是"当前技术栈内的最佳工程解"，不是理论最优。边界与升级路径如下：

| 维度 | 现状 |
|---|---|
| 稳定性 | ✅ glow 3/3 冻结 → wgpu+SW 0/3（MPO 排除后复测通过）；架构上消除 mpv 与窗口 GPU 上下文耦合，呈现层缺陷彻底绕开 |
| 性能 | 1080p24 实测 CPU ~3.5%（12 核）/内存 ~300MB；**SW 是 CPU 回读路径，4K60 数据量 ×10（约 2GB/s）预估 CPU 10~15% + 内存带宽压力** |
| 画质 | rgb0 为 8bit RGB，HDR 片源被 mpv tone-map 到 SDR，**无 HDR 直通** |
| 根本约束 | mpv render API 仅支持 GL/SW 两种输出 target；在 wgpu (D3D12) 后端下无官方零拷贝通道，SW 是唯一干净路径 |

**升级触发条件**（满足其一即启动路线 B，无需提前）：

- 4K60 片源 CPU 占用过高或掉帧
- HDR 直通需求
- 多路同时播放

**路线 B：GPU 零拷贝共享纹理**——mpv 渲染到 D3D11 shared texture → wgpu 侧 import，彻底消除 GPU→CPU 回读。切换成本可控：业务层（VFS/proxy/store/弹幕 UI）与渲染内核已解耦。**（已实施为 wid GPU 路径）**

短期低成本优化（不动架构）：`render_sw` 按需渲染已实现（`request_update() & MPV_RENDER_UPDATE_FRAME`）；吃紧时可加纹理双缓冲或上传线程化。

---

## 编译与运行

```bash
# 开发构建 + 运行（cargo run 的 cwd 为 workspace 根，
# DLL 放在 dyplayer/ 下即可被搜索到）
cargo run -p dyplayer-app

# 发布构建（thin LTO）
cargo build --release -p dyplayer-app

# 老设备版（glow/OpenGL，SW-only，Win7+ + DX9 级硬件）
cargo build --release --no-default-features --features backend-glow

# 双版本打包 → dist/dyplayer-<ver>-win-x64.zip + dist/dyplayer-<ver>-win-x64-legacy.zip
powershell -ExecutionPolicy Bypass -File scripts/package.ps1

# 运行全部测试（基线 510+，全绿是提交前提）
cargo test --workspace

# clippy（零警告是提交前提）
cargo clippy --workspace
```

### 环境要求

- Rust stable（edition 2024，需 1.85+）
- Windows 10/11（首要），Linux/macOS 保持可编译
- [libmpv](https://sourceforge.net/projects/mpv-player-windows/files/libmpv/) 开发包（`mpv-dev`）

主要依赖版本：eframe/egui/epaint 0.37（wgpu 后端）、windows-rs 0.62、rusqlite 0.37（bundled SQLite）。

### 获取 libmpv

`vendor/mpv-dev/` 已含 mpv 头文件与导入库，但 **DLL（约 115MB）不入库**（见 `.gitignore`）。首次使用需自行放置：

```bash
# 方式一：从 mpv-dev 压缩包解压 libmpv-2.dll 到 vendor/mpv-dev/
# 方式二：放任意位置，用环境变量指向
set DYPLAYER_LIBMPV=D:\path\to\libmpv-2.dll
```

运行时搜索顺序：`DYPLAYER_LIBMPV` 环境变量 → `config.toml` 的 `engine.libmpv_path` → 可执行文件目录 → 当前工作目录 → 系统 PATH。

---

## Windows 分发依赖与启动失败排查

> 现象：打包后的 `dyplayer.exe` + `libmpv-2.dll` 在部分 Win10 机器上报 **「启动播放引擎失败: loading libmpv」**，开发机 Win11 一切正常、无法复现。

### 根因：libmpv-2.dll 静态硬依赖 vulkan-1.dll

对 `vendor/mpv-dev/libmpv-2.dll`（mpv-winbuild/shinchiro 构建 `v0.41.0-1122-gaccb7b282`）解析 PE 导入表（DataDirectory[1]），在 48 个依赖 DLL 中，除 `vulkan-1.dll` 外其余（UCRT、`api-ms-win-core-*`、d2d1/DWrite/SHCORE 等）Win10 均自带：

| 依赖 | 性质 | Win10 默认 |
|---|---|---|
| `vulkan-1.dll` | **Vulkan ICD loader，非 OS 组件**，由显卡驱动 / Vulkan Runtime 附带 | 大量机器**缺失** |
| `VCRUNTIME140.dll`（exe 自身依赖） | MSVC 动态 CRT，exe 静态导入 | 需 VC++ 2015-2022 redist |
| UCRT / api-ms-win-* / d2d1 / DWrite 等 | 系统组件 | 内置 |

**失败链路**：`LoadLibraryW("libmpv-2.dll")` → 加载器解析静态导入表 → 找不到 `vulkan-1.dll` → 直接失败（OS error 126，模块根本进不了符号解析）→ `ffi.rs` `FfiError::DllLoad` → `player_app.rs` `context("loading libmpv")` → 应用内弹出该错误。mpv-winbuild 启用 libplacebo + Vulkan，`vulkan-1.dll` 因此成为链接期依赖；而 DyPlayer 实际只走 OpenGL render API，该依赖纯属"上游构建产物拖累"。

**开发机为何复现不了**：Win11 开发机的驱动普遍自带 `vulkan-1.dll`（System32 有），Win10 目标机没有——这是典型的"开发环境依赖噪声"问题。

### 排查方法（按序）

1. 看错误尾部文案，区分两类失败：
   - `failed to load ... (os error 126)` → 依赖 DLL 缺失（本节根因）
   - `not found in any candidate path: ...` → exe 旁没放 dll（搜索序见上文「获取 libmpv」）
2. 检查目标机 `C:\Windows\System32\vulkan-1.dll` 是否存在
3. 实锤验证：把开发机的 `vulkan-1.dll`（x64）拷到 exe 旁再启动。`ffi.rs` 加载已用 `LOAD_WITH_ALTERED_SEARCH_PATH`，exe 同目录可被依赖解析命中

### 分发修复方向

| 措施 | 说明 | 状态 |
|---|---|---|
| **随包分发 `vulkan-1.dll`** | 纯 ICD loader（~1.5 MB，Khronos 官方分发物）。无显卡 ICD 时仅 Vulkan 路径不可用（`vkCreateInstance` 失败），**不影响 OpenGL render API 播放** | 已实施（package.ps1 打包含 vulkan-1.dll） |
| 自编译裁剪 libmpv（禁 Vulkan） | 从根上消除依赖，见下节裁剪方案 A；裁剪指纹分析中 Vulkan 全栈（347 指纹）本就判为纯冗余 | 长期 |
| exe 附带 `VCRUNTIME140.dll` 或改 `+crt-static` | Win10 裸机无 VC++ redist 时 exe 本体起不来（此情形下连应用内错误框都看不到） | 待实施 |

---

## 发布体积与裁剪分析

> 以下为实测分析结论（2026-10，mpv v0.41.0-1122 / FFmpeg N-127313），**尚未实施自编译裁剪**，作为后续发布方案依据。

### 现状（实测）

| 组件 | 大小 | 占比 |
|---|---|---|
| `libmpv-2.dll`（官方 mpv-dev，FFmpeg 静态内置） | 115.3 MB | 85% |
| `dyplayer.exe`（release, thin LTO） | 19.9 MB | 15% |
| **发布合计** | **~135 MB** | |
| DLL zip（deflate）实测 | 44.9 MB | 压缩率 39% |

### DLL 冗余成分（字符串指纹扫描）

DyPlayer 走 OpenGL render API + 自绘 UI，官方全量构建中以下组件用不到：

| 组件 | 指纹数 | 说明 |
|---|---|---|
| Vulkan 全栈（vulkan/spirv/shaderc/glslang） | 347/685/38/12 | vo_gpu_next 的 Vulkan 路径，纯冗余，最大头之一 |
| CUDA/NVDEC | 135/32 | N 卡硬解路径（可选保留） |
| VAAPI | 106 | Linux 死代码 |
| libbluray / dvdnav | 118/22 | 蓝光/DVD 导航，用不到 |
| Lua / JavaScript | 204/22 | mpv 脚本系统（osc/stats 等），嵌入式场景不需要 |
| VPX/uchardet/caca | 206/4/1 | 可裁或按需 |

**不可裁**：libplacebo（render API 的 GL 路径依赖，但可只编 GL 后端禁 Vulkan）、d3d11/dxgi/dxva2（Windows 硬解主路径）、libass+freetype+harfbuzz（字幕轨渲染）、FFmpeg 常用 codec（h264/hevc/av1/vp9/aac/opus/flac/mp3/ac3/eac3/dts）与常用容器（mp4/mkv/webm/ts/flv/avi）。

### 裁剪方案对比

| 方案 | 分发体积 | 磁盘占用 | 风险/成本 |
|---|---|---|---|
| **A. 自编译精简 libmpv**（msys2/meson：禁 vulkan/lua/js/bluray/dvd/vapoursynth，FFmpeg `--disable-everything` 按需启用） | ~20 MB | **~35-50 MB** | 中：需维护构建脚本跟随上游；裁 codec 过狠会导致部分视频无法播放 |
| **B. 原样 DLL + 压缩分发**（zip 实测 45 MB；7z/LZMA 估 38-42 MB） | **~50-55 MB** | 135 MB | 零风险，立即生效 |
| C. UPX 压缩 DLL | ~45 MB | ~45 MB | 高：大 DLL 杀软误报率高，SEH/TLS 兼容性隐患，不推荐 |
| D. exe 瘦身（`strip=true` + `panic="abort"` + `codegen-units=1` + `lto="fat"`） | -5~7 MB | 同左 | 低：egui 应用常规操作 |

**结论**：85% 体积在 DLL，单靠 exe 瘦身（D）只能省零头，**只有自编译（A）能真正裁剪**，115 MB 可压到 35-50 MB；求稳可先走 B。A+D 组合最终分发包可到 ~20 MB、磁盘 ~55 MB。

**裁剪后必须回归验证**：

- `mpv --decoders` 对比保留清单，确认常用格式可解
- 硬解链路（d3d11va → GL interop）实际播放验证
- HTTP Range 代理路径需保留 FFmpeg 的 http/tcp 协议模块

---

## 代码签名

> 背景：无签名、零传播量的 Rust exe 是 360 QVM 启发式引擎的高危图像（动态加载 DLL、本地 HTTP 代理、写 keyring、SMB 连接等合法行为与恶意软件特征重合），发布版会被误报。代码签名是治本手段，配合分发量与时间让云信誉爬坡。

### 证书路线（推荐）

| 阶段 | 方案 | 成本 | 说明 |
|---|---|---|---|
| 当前（开源项目） | **Certum 开源代码签名证书**（首选） | ~€69/年 | 门槛：项目开源，DyPlayer 符合；走 SimplySign 云签名 |
| 当前（备选） | SignPath 开源免费计划 | 免费 | 需向 SignPath Foundation 申请获批，有签名配额 |
| 商业化后 | OV 证书（Sectigo/DigiCert） | ¥1500~3000/年 | 需企业营业执照 |
| 商业化后（最优） | EV 证书 | ¥4000+/年 | SmartScreen/杀软信誉即时生效 |

**合规注意**：2023-06 起 CA/B 新规强制私钥入硬件（USB Token/HSM/云 HSM），不再签发可导出 PFX。Certum 走 SimplySign 云端 HSM，本机安装其中间件后 signtool 按证书主题名直接调用，无本地私钥文件。

### 签名命令

signtool 随 Windows SDK 分发。**RFC 3161 时间戳必须加**——它证明签名发生在证书有效期内，证书过期后已分发的 exe 仍保持有效：

```cmd
signtool sign /fd SHA256 ^
  /tr http://timestamp.digicert.com /td SHA256 ^
  /n "证书主题名" ^
  target\release\dyplayer-app.exe

:: 验证签名链与时间戳
signtool verify /pa /v target\release\dyplayer-app.exe
```

### 构建链集成

签名与 exe 哈希绑定，**每次构建后必须重签**，流程固定为：构建 → 签名 → 打包分发：

```cmd
cargo build --release -p dyplayer-app
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 ^
  /n "证书主题名" target\release\dyplayer-app.exe
```

后续脚本化发布时，签名步骤插入 build 与 zip 打包之间，禁止漏签分发。

### 要点

- **只签主 exe 即可**（杀软启发式主要看主程序）；`libmpv-2.dll` 等附属文件不强制
- **签名 ≠ 立即免报**：初期仍可能被 QVM 拦截，需"签名 + 分发量 + 时间"让云信誉爬坡；期间通过 360 官方误报申诉通道提交样本与哈希加白
- **禁止 UPX 加壳**（与裁剪方案 C 结论一致）：加壳是误报强特征，得不偿失
- CI 场景可平滑迁移：SignPath 提供 GitHub Actions 集成；若后续转 Azure Trusted Signing 有官方 action，证书身份走 OIDC 无 secrets 泄漏风险

---
