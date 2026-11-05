# dyplayer-desktop

面向"已经自己存好视频"用户的 Rust 桌面播放器：不做刮削、不做元数据匹配，**文件夹结构就是媒体库**。

播放内核为 libmpv（render API 内嵌），UI 基于 egui/eframe，弹幕采用 DFM+ 布局内核（vendored 上游实现）+ glyph atlas 自绘渲染。

- 目标平台：Windows 10/11（首要），Linux/macOS 保持可编译（未实施）

## 功能特性

- **本地源媒体库**：文件夹层级浏览、面包屑导航、自然排序（`ep1 < ep2 < ep10`）、文件夹即播放列表
- **远程源**：WebDAV/SMB 源浏览播放（凭证入 keyring）、本地 HTTP Range 流代理
- **mpv 播放**：内嵌 render API 渲染、暂停/恢复、进度条拖拽 seek、上一集/下一集、自动连播、队列面板
- **续播与最近播放**：播放位置按 5s 间隔入库，重启后从上次位置恢复；最近播放倒序展示进度，点击续播
- **字幕**：自动加载同名字幕；GBK 编码自动转码为 UTF-8 并缓存
- **DFM+ 弹幕**：自动加载同名 B 站 XML 弹幕，经 DFM+ 布局引擎 prepare 后在应用内自绘（滚动/顶部/底部、描边、透明度、字号、显示区域、延迟等设置）

## 渲染路径

播放器支持三条渲染路径，按需选择：

| 路径 | 适用 | 配置 |
|---|---|---|
| `sw`（默认） | 通用兼容 | `engine.render_path="sw"` |
| `gpu` | 8K AV1 / 高码率，CPU 全留解码 | `engine.render_path="gpu"` |
| `glow` (legacy) | 老设备 (Win7+/DX9) | `--features backend-glow` 编译 |

## 环境要求

- Windows 10/11
- [libmpv](https://sourceforge.net/projects/mpv-player-windows/files/libmpv/) 开发包（`mpv-dev`），或使用随包分发的 DLL

## 获取 libmpv

`vendor/mpv-dev/` 已含 mpv 头文件与导入库，但 **DLL（约 115MB）不入库**（见 `.gitignore`）。首次使用需自行放置：

```bash
# 方式一：从 mpv-dev 压缩包解压 libmpv-2.dll 到 vendor/mpv-dev/
# 方式二：放任意位置，用环境变量指向
set DYPLAYER_LIBMPV=D:\path\to\libmpv-2.dll
```

运行时搜索顺序：`DYPLAYER_LIBMPV` 环境变量 → `config.toml` 的 `engine.libmpv_path` → 可执行文件目录 → 当前工作目录 → 系统 PATH。

## 编译与运行

```bash
# 开发构建 + 运行
cargo run -p dyplayer-app

# 发布构建
cargo build --release -p dyplayer-app

# 老设备版（glow/OpenGL，SW-only）
cargo build --release --no-default-features --features backend-glow

# 双版本打包 → dist/dyplayer-<ver>-win-x64.zip + dist/dyplayer-<ver>-win-x64-legacy.zip
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

构建需 Rust stable（edition 2024，需 1.85+）。

## 配置与数据目录

| 路径 | 内容 |
|---|---|
| `%APPDATA%\dyplayer\config.toml` | 应用配置（引擎、播放行为、弹幕设置等） |
| `%LOCALAPPDATA%\dyplayer\library.db` | SQLite 媒体库（源、播放历史） |
| `%LOCALAPPDATA%\dyplayer\cache\subs\` | GBK 字幕转码缓存 |
| `%LOCALAPPDATA%\dyplayer\logs\` | 日志（按天轮转，保留 7 天） |

日志可用 `RUST_LOG` 环境变量控制级别（如 `RUST_LOG=dyplayer=debug`）。

## 里程碑

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 | libmpv render API + egui 同帧合成打通 | ✅ 完成 |
| M1 | 本地源 MVP：浏览→播放→控制→连播→续播→最近播放 | ✅ 完成 |
| M2 | DFM+ 弹幕引擎：vendored 布局核心 + XML 规范化 + glyph atlas 自绘 | ✅ 完成 |
| M3 | 远程源：WebDAV/SMB 播放、keyring 凭证、HTTP Range 代理 | ✅ 完成 |
| M4+ | 收藏、播放器设置完整化、发布工程（签名/裁剪分发） | 进行中 |

## 开发者文档

架构、设计决策、渲染管线、发布排查、体积裁剪、代码签名等开发者向内容见 [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)。
