use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 解码图片字节（jpeg/png）→ egui ColorImage（RGBA 非预乘）。
/// image crate 0.25 default-features=false + jpeg/png，load_from_memory 按
/// magic bytes 推断格式；失败返回 None（调用方存 CoverState::Failed 不重试）。
pub(crate) fn decode_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &rgba.into_raw(),
    ))
}

/// 抽帧请求（从 main.rs 帧末 spawn 传入后台线程）。
pub struct ThumbRequest {
    pub source_id: i64,
    pub folder_path: String,
    pub source: crate::browser::MediaSource,
    pub libmpv_path: Option<PathBuf>,
    pub thumbs_dir: PathBuf,
}

/// 抽帧结果（后台线程 → main.rs poll）。
pub struct ThumbResult {
    pub key: String,
    pub image: Option<egui::ColorImage>,
}

/// 缩略图缓存 key：`thumb://{source_id}/{folder_path}`
pub fn thumb_key(source_id: i64, folder_path: &str) -> String {
    format!("thumb://{source_id}/{folder_path}")
}

/// 缩略图磁盘缓存路径：`thumbs/{source_id}/{hash(folder_path)}.jpg`
fn thumb_disk_path(thumbs_dir: &Path, source_id: i64, folder_path: &str) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    folder_path.hash(&mut hasher);
    let hash = hasher.finish();
    thumbs_dir
        .join(format!("{source_id}"))
        .join(format!("{hash:016x}.jpg"))
}

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v", "mpg", "mpeg", "ts", "m2ts",
];

/// 从文件夹中找第一个视频文件（用于抽帧）。
fn find_first_video(entries: &[dyplayer_vfs::VfsEntry]) -> Option<&dyplayer_vfs::VfsEntry> {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .find(|e| {
            e.path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| VIDEO_EXTS.contains(&ext.to_lowercase().as_str()))
        })
}

/// 等待 mpv render context 有新帧（update callback + request_update）。
/// 返回 true = 有帧可渲染，false = 超时。
fn wait_frame(
    frame_ready: &Arc<AtomicBool>,
    render: &dyplayer_mpv::MpvRenderContext,
    client: &dyplayer_mpv::MpvClient,
    deadline: Instant,
    tkey: &str,
    phase: &str,
) -> bool {
    let mut last_diag = Instant::now();
    while Instant::now() < deadline {
        if frame_ready.swap(false, Ordering::Relaxed) {
            let flags = render.request_update();
            if flags & dyplayer_mpv::MPV_RENDER_UPDATE_FRAME != 0 {
                return true;
            }
        }
        if last_diag.elapsed() > Duration::from_secs(2) {
            let idle = client.get_property_flag("idle-active").unwrap_or(true);
            let filename = client.get_property_string("filename").unwrap_or_default();
            let duration = client.get_property_f64("duration").unwrap_or(-1.0);
            tracing::warn!(
                "thumb_gen[{tkey}] {phase}诊断: idle={idle} filename=\"{filename}\" duration={duration}"
            );
            last_diag = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    tracing::warn!("thumb_gen[{tkey}] {phase}等帧超时");
    false
}

/// headless mpv 抽帧核心逻辑。
///
/// 三级缩略图来源中的第二级（mpv headless 抽帧）：
/// 1. 检查磁盘缓存命中 → 直接读 jpg 解码
/// 2. open_vfs + list_dir 找第一个视频文件
/// 3. 构建 loadfile URL（本地直连 / 远程 proxy）
/// 4. 创建 headless mpv（vo=libmpv + ao=null + SW render context）
/// 5. loadfile + 等帧 + seek + 等帧 + screenshot-to-file
/// 6. 原子写（临时文件 + rename）
/// 7. 读 jpg → decode_image
///
/// 超时 15s，失败返回 None（调用方存 Failed 不重试）。
/// vo=libmpv + render context 是验证过的 headless 路径（sw_pacing_verify.rs）：
/// vo=null 下无消费者不拉流，duration 拿不到值。
pub fn extract_thumb(req: &ThumbRequest) -> Option<egui::ColorImage> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let disk_path = thumb_disk_path(&req.thumbs_dir, req.source_id, &req.folder_path);
    let tkey = thumb_key(req.source_id, &req.folder_path);

    // 1. 磁盘缓存命中
    if disk_path.exists() {
        if let Ok(bytes) = std::fs::read(&disk_path)
            && let Some(img) = decode_image(&bytes)
        {
            tracing::debug!("thumb_gen[{tkey}] 磁盘缓存命中");
            return Some(img);
        }
        let _ = std::fs::remove_file(&disk_path);
    }

    // 2. 判断 folder_path 是视频文件还是目录：视频文件直接 loadfile，
    //    文件夹则 list_dir 找第一个视频。
    let (vfs, _) = match req.source.open_vfs() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("thumb_gen[{tkey}] open_vfs 失败: {e}");
            return None;
        }
    };
    let video_path = if Path::new(&req.folder_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| VIDEO_EXTS.contains(&ext.to_lowercase().as_str()))
    {
        PathBuf::from(&req.folder_path)
    } else {
        let entries = match vfs.list_dir(Path::new(&req.folder_path)) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("thumb_gen[{tkey}] list_dir 失败: {e}");
                return None;
            }
        };
        match find_first_video(&entries) {
            Some(v) => v.path.clone(),
            None => {
                tracing::warn!("thumb_gen[{tkey}] 未找到视频文件");
                return None;
            }
        }
    };
    tracing::info!("thumb_gen[{tkey}] 抽帧目标: {}", video_path.display());

    // 3. 构建 loadfile URL（proxy 必须存活到 mpv 加载完成，不能在 if 块内 drop）
    let proxy = if req.source.is_remote() {
        match dyplayer_proxy::StreamProxy::spawn(vfs.clone()) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("thumb_gen[{tkey}] proxy 启动失败: {e}");
                return None;
            }
        }
    } else {
        None
    };
    let play_url = match &proxy {
        Some(p) => p.url_for(&video_path),
        None => video_path.to_string_lossy().into_owned(),
    };

    // 4. 创建 headless mpv（vo=libmpv + ao=null + SW render context）
    let lib = req
        .libmpv_path
        .as_ref()
        .and_then(|p| dyplayer_mpv::MpvLibrary::load(Some(p)).ok())
        .or_else(|| dyplayer_mpv::MpvLibrary::load(None).ok())?;

    let options = dyplayer_mpv::ClientOptions {
        hwdec: "no".into(),
        extra_options: vec![
            ("ao".into(), "null".into()),
            ("screenshot-format".into(), "jpg".into()),
            ("screenshot-jpeg-quality".into(), "70".into()),
            ("screenshot-tag-colorspace".into(), "no".into()),
        ],
    };
    let client = match dyplayer_mpv::MpvClient::create(lib, &options) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("thumb_gen[{tkey}] mpv 创建失败: {e}");
            return None;
        }
    };
    let mut render = match unsafe { dyplayer_mpv::MpvRenderContext::create_sw(&client) } {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("thumb_gen[{tkey}] render context 创建失败: {e}");
            return None;
        }
    };

    // update callback：mpv 有新帧时设置标志
    let frame_ready = Arc::new(AtomicBool::new(false));
    let frame_ready_cb = Arc::clone(&frame_ready);
    render.set_update_callback(Box::new(move || {
        frame_ready_cb.store(true, Ordering::Relaxed);
    }));

    // 5. loadfile + 等帧
    if let Err(e) = client.load_file(&play_url) {
        tracing::warn!("thumb_gen[{tkey}] loadfile 失败: {e}");
        return None;
    }
    if !wait_frame(&frame_ready, &render, &client, deadline, &tkey, "加载") {
        return None;
    }

    // 6. seek 到 min(10s, 10%时长)
    let duration = client.get_property_f64("duration").unwrap_or(0.0);
    let seek_target = if duration > 0.0 {
        (duration * 0.1).clamp(1.0, 10.0)
    } else {
        5.0
    };
    tracing::info!("thumb_gen[{tkey}] seek 到 {seek_target:.1}s (duration={duration:.1}s)");
    frame_ready.store(false, Ordering::Relaxed);
    if let Err(e) = client.command(&["seek", &format!("{seek_target:.3}"), "absolute"]) {
        tracing::warn!("thumb_gen[{tkey}] seek 失败: {e}");
        return None;
    }
    if !wait_frame(&frame_ready, &render, &client, deadline, &tkey, "seek") {
        return None;
    }

    // 7. screenshot-to-file（原子写：临时文件 + rename）
    let tmp_path = disk_path.with_extension("jpg.tmp");
    if let Some(parent) = disk_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Err(e) = client.command(&[
        "screenshot-to-file",
        &tmp_path.to_string_lossy(),
        "video",
    ]) {
        tracing::warn!("thumb_gen[{tkey}] screenshot 失败: {e}");
        return None;
    }

    // 等截图写入完成
    std::thread::sleep(Duration::from_millis(300));

    if tmp_path.exists() {
        let _ = std::fs::rename(&tmp_path, &disk_path);
    }

    // 8. 读 jpg → decode
    if disk_path.exists()
        && let Ok(bytes) = std::fs::read(&disk_path)
    {
        match decode_image(&bytes) {
            Some(img) => {
                tracing::info!("thumb_gen[{tkey}] 抽帧成功 → {}", disk_path.display());
                return Some(img);
            }
            None => {
                tracing::warn!("thumb_gen[{tkey}] 截图文件解码失败");
                return None;
            }
        }
    }

    tracing::warn!("thumb_gen[{tkey}] 截图文件未生成");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_key_stability() {
        let k1 = thumb_key(5, "/video/伊介书生");
        let k2 = thumb_key(5, "/video/伊介书生");
        assert_eq!(k1, k2);
        let k3 = thumb_key(6, "/video/伊介书生");
        assert_ne!(k1, k3);
    }

    #[test]
    fn disk_path_stability() {
        let dir = PathBuf::from("/tmp/thumbs");
        let p1 = thumb_disk_path(&dir, 5, "/video/test");
        let p2 = thumb_disk_path(&dir, 5, "/video/test");
        assert_eq!(p1, p2);
        let p3 = thumb_disk_path(&dir, 5, "/video/other");
        assert_ne!(p1, p3);
        let p4 = thumb_disk_path(&dir, 6, "/video/test");
        assert_ne!(p1, p4);
    }

    #[test]
    fn find_first_video_basic() {
        let entries = vec![
            dyplayer_vfs::VfsEntry {
                name: "cover.jpg".into(),
                path: PathBuf::from("/dir/cover.jpg"),
                is_dir: false,
                size: 100,
                modified: None,
            },
            dyplayer_vfs::VfsEntry {
                name: "ep01.mp4".into(),
                path: PathBuf::from("/dir/ep01.mp4"),
                is_dir: false,
                size: 1000,
                modified: None,
            },
            dyplayer_vfs::VfsEntry {
                name: "subdir".into(),
                path: PathBuf::from("/dir/subdir"),
                is_dir: true,
                size: 0,
                modified: None,
            },
        ];
        let v = find_first_video(&entries).expect("should find ep01.mp4");
        assert_eq!(v.name, "ep01.mp4");
    }

    #[test]
    fn find_first_video_none() {
        let entries = vec![dyplayer_vfs::VfsEntry {
            name: "cover.jpg".into(),
            path: PathBuf::from("/dir/cover.jpg"),
            is_dir: false,
            size: 100,
            modified: None,
        }];
        assert!(find_first_video(&entries).is_none());
    }
}