use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use dyplayer_store::Store;
use dyplayer_vfs::{
    VfsEntry, VfsSource, is_subtitle_file, is_video_file, match_subtitles_for, natural_cmp,
};

use crate::player_app::{MediaItem, PendingPlay};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserView {
    Files,

}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Local,
    WebDav,
    Smb,
}

impl SourceKind {
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Self::Local),
            "webdav" => Some(Self::WebDav),
            "smb" => Some(Self::Smb),
            _ => None,
        }
    }

    pub fn as_db(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::WebDav => "webdav",
            Self::Smb => "smb",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "本地",
            Self::WebDav => "WebDAV",
            Self::Smb => "SMB",
        }
    }

}

/// 统一媒体源（本地文件夹 / WebDAV / SMB）。
/// `endpoint` 语义按 kind：local=根路径、webdav=base_url、smb=`smb://host/share[/sub]`。
/// `root` 仅 webdav 使用（源内起始 vpath，空 = base_url 根）。
#[derive(Debug, Clone)]
pub struct MediaSource {
    pub id: i64,
    pub name: String,
    pub kind: SourceKind,
    pub endpoint: String,
    pub username: String,
    pub root: PathBuf,
    pub accept_invalid_certs: bool,
}

impl MediaSource {
    pub fn is_remote(&self) -> bool {
        self.kind != SourceKind::Local
    }

    /// 构建 VFS 实例与浏览起始目录。远程源凭证从 keyring 读取。
    pub fn open_vfs(&self) -> Result<(Arc<dyn VfsSource>, PathBuf), String> {
        if self.kind == SourceKind::Local {
            return self.open_vfs_with_password("");
        }
        // keyring 读取失败必须显式报错（静默空密码会让用户在 401 里猜原因）；
        // 填了用户名却没有已存密码，几乎必然是凭证丢失（如 keyring 后端异常），
        // 引导用户去编辑源重新填写；用户名为空（匿名访问）允许空密码。
        let password = match dyplayer_store::secrets::get_source_credential(self.id) {
            Ok(Some(pw)) => pw,
            Ok(None) if self.username.is_empty() => String::new(),
            Ok(None) => {
                return Err(format!(
                    "源「{}」没有已保存的密码（凭据可能未写入或已丢失），请编辑该源重新填写密码",
                    self.name
                ));
            }
            Err(e) => return Err(format!("读取已存凭据失败: {e}")),
        };
        self.open_vfs_with_password(&password)
    }

    /// 同 `open_vfs`，但显式注入密码（连接测试时表单密码尚未入库）。
    pub fn open_vfs_with_password(
        &self,
        password: &str,
    ) -> Result<(Arc<dyn VfsSource>, PathBuf), String> {
        match self.kind {
            SourceKind::Local => {
                let fs = dyplayer_vfs::LocalFs::new(&self.root).map_err(|e| e.to_string())?;
                let root = fs.root().to_path_buf();
                Ok((Arc::new(fs), root))
            }
            SourceKind::WebDav => {
                let fs = dyplayer_vfs::WebDavFs::new(dyplayer_vfs::WebDavConfig {
                    base_url: self.endpoint.clone(),
                    username: self.username.clone(),
                    password: password.to_string(),
                    bearer: None,
                    accept_invalid_certs: self.accept_invalid_certs,
                });
                let root = if self.root.as_os_str().is_empty() {
                    PathBuf::from("/")
                } else {
                    self.root.clone()
                };
                Ok((Arc::new(fs), root))
            }
            SourceKind::Smb => {
                let fs = dyplayer_vfs::SmbFs::new(dyplayer_vfs::SmbConfig {
                    endpoint: self.endpoint.clone(),
                    username: self.username.clone(),
                    password: password.to_string(),
                    domain: String::new(),
                })
                .map_err(|e| e.to_string())?;
                let root = fs.root().to_path_buf();
                Ok((Arc::new(fs), root))
            }
        }
    }
}

/// 后台目录请求的结果。
pub enum DirOutcome {
    /// 目录浏览完成（用户停留视图更新）。
    BrowseLoaded {
        dir: PathBuf,
        entries: Vec<VfsEntry>,
    },
    /// 目录浏览失败（保留旧列表，展示错误）。
    BrowseFailed { error: String },
    /// "播放文件夹 / 恢复最近播放"完成，直接开播。
    FolderPlayReady(Box<PendingPlay>),
    /// "播放文件夹 / 恢复最近播放"失败。
    FolderPlayFailed { error: String },
}

pub struct BrowserState {
    pub view: BrowserView,
    pub active_source: Option<usize>,
    /// 当前选中源的 DB id（收藏按 (source_id, path) 定位）。
    pub active_source_id: Option<i64>,
    pub dir: PathBuf,
    pub entries: Vec<VfsEntry>,
    pub error: Option<String>,
    pub recent: Vec<dyplayer_store::RecentEntry>,
    /// 收藏视图数据（全部源，按时间倒序）。
    pub favorites: Vec<dyplayer_store::FavoriteEntry>,
    /// 正在加载的目录（异步 list_dir 进行中）。
    loading_dir: Option<PathBuf>,
    browse_rx: Option<mpsc::Receiver<DirOutcome>>,
    folder_play_rx: Option<mpsc::Receiver<DirOutcome>>,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self {
            view: BrowserView::Files,
            active_source: None,
            active_source_id: None,
            dir: PathBuf::new(),
            entries: Vec::new(),
            error: None,
            recent: Vec::new(),
            favorites: Vec::new(),
            loading_dir: None,
            browse_rx: None,
            folder_play_rx: None,
        }
    }
}

impl BrowserState {
    pub fn refresh_recent(&mut self, store: &Store) {
        match store.recent_played(50) {
            Ok(entries) => {
                self.recent = entries;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("读取最近播放失败: {e}")),
        }
    }

    /// 刷新收藏数据（全部源，按时间倒序）。
    pub fn refresh_favorites(&mut self, store: &Store) {
        match store.list_favorites(None) {
            Ok(all) => {
                self.favorites = all;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("读取收藏失败: {e}")),
        }
    }

    /// 设置活跃源索引并同步其 DB id（收藏星标/收藏夹跳转依赖两者一致）。
    /// 调用方在切换后按需调用 `refresh_favorites` 更新星标集合。
    pub fn set_active_source(&mut self, idx: Option<usize>, sources: &[MediaSource]) {
        self.active_source = idx;
        self.active_source_id = idx.and_then(|i| sources.get(i)).map(|s| s.id);
    }

    pub fn is_loading(&self) -> bool {
        self.loading_dir.is_some() || self.folder_play_rx.is_some()
    }

    /// 异步浏览目录：后台线程 list_dir，结果经 `poll_dir` 回到 UI。
    pub fn request_dir(&mut self, source: &MediaSource, dir: PathBuf, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let snapshot = source.clone();
        let req_dir = dir.clone();
        let repaint = ctx.clone();
        std::thread::Builder::new()
            .name("dyplayer-browse".into())
            .spawn(move || {
                let outcome = match snapshot.open_vfs() {
                    Ok((vfs, _)) => match vfs.list_dir(&req_dir) {
                        Ok(mut entries) => {
                            entries.retain(is_listable_entry);
                            sort_entries(&mut entries);
                            DirOutcome::BrowseLoaded {
                                dir: req_dir,
                                entries,
                            }
                        }
                        Err(e) => DirOutcome::BrowseFailed {
                            error: e.to_string(),
                        },
                    },
                    Err(e) => DirOutcome::BrowseFailed { error: e },
                };
                let _ = tx.send(outcome);
                repaint.request_repaint();
            })
            .ok();
        self.loading_dir = Some(dir);
        self.error = None;
        self.browse_rx = Some(rx);
    }

    /// 异步"播放文件夹"：后台 list_dir + 组队列。
    /// `clicked` = Some 时定位该文件（最近播放恢复）；None 时从第一个视频开始。
    pub fn request_folder_play(
        &mut self,
        source: &MediaSource,
        dir: PathBuf,
        clicked: Option<PathBuf>,
        ctx: &egui::Context,
    ) {
        let (tx, rx) = mpsc::channel();
        let snapshot = source.clone();
        let req_dir = dir.clone();
        let repaint = ctx.clone();
        std::thread::Builder::new()
            .name("dyplayer-folder-play".into())
            .spawn(move || {
                let outcome = match snapshot.open_vfs() {
                    Ok((vfs, root)) => {
                        let req_dir = resolve_play_dir(&req_dir, &root);
                        match vfs.list_dir(&req_dir) {
                            Ok(mut entries) => {
                                sort_entries(&mut entries);
                                let target = match &clicked {
                                    Some(want) => entries
                                        .iter()
                                        .find(|e| {
                                            !e.is_dir && is_video_file(&e.path) && e.path == *want
                                        })
                                        .map(|e| e.path.clone()),
                                    None => entries
                                        .iter()
                                        .find(|e| !e.is_dir && is_video_file(&e.path))
                                        .map(|e| e.path.clone()),
                                };
                                match target {
                                    Some(first) => {
                                        let mut pending =
                                            build_playlist(snapshot.id, &entries, &first);
                                        if snapshot.is_remote() {
                                            pending.remote = Some(snapshot);
                                        }
                                        DirOutcome::FolderPlayReady(Box::new(pending))
                                    }
                                    None => DirOutcome::FolderPlayFailed {
                                        error: if clicked.is_some() {
                                            "文件已不存在".into()
                                        } else {
                                            "该文件夹下没有视频".into()
                                        },
                                    },
                                }
                            }
                            Err(e) => DirOutcome::FolderPlayFailed {
                                error: e.to_string(),
                            },
                        }
                    }
                    Err(e) => DirOutcome::FolderPlayFailed { error: e },
                };
                let _ = tx.send(outcome);
                repaint.request_repaint();
            })
            .ok();
        self.folder_play_rx = Some(rx);
    }

    /// 每帧轮询后台请求结果；返回待播放的队列（如有）。
    pub fn poll_dir(&mut self, ctx: &egui::Context) -> Option<PendingPlay> {
        let mut ready_play = None;
        if let Some(rx) = &self.browse_rx {
            loop {
                match rx.try_recv() {
                    Ok(DirOutcome::BrowseLoaded { dir, entries }) => {
                        self.entries = entries;
                        self.dir = dir;
                        self.error = None;
                        self.loading_dir = None;
                        self.view = BrowserView::Files;
                    }
                    Ok(DirOutcome::BrowseFailed { error }) => {
                        self.error = Some(error);
                        self.loading_dir = None;
                    }
                    Ok(other) => {
                        // browse 通道不会收到 FolderPlay 结果；防御性忽略
                        let _ = other;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        ctx.request_repaint_after(Duration::from_millis(80));
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.browse_rx = None;
                        self.loading_dir = None;
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &self.folder_play_rx {
            loop {
                match rx.try_recv() {
                    Ok(DirOutcome::FolderPlayReady(pending)) => {
                        ready_play = Some(*pending);
                    }
                    Ok(DirOutcome::FolderPlayFailed { error }) => {
                        self.error = Some(error);
                    }
                    Ok(other) => {
                        let _ = other;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        ctx.request_repaint_after(Duration::from_millis(80));
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.folder_play_rx = None;
                        break;
                    }
                }
            }
        }
        ready_play
    }
}

fn is_listable_entry(entry: &VfsEntry) -> bool {
    entry.is_dir || is_video_file(&entry.path) || is_subtitle_file(&entry.path)
}

/// 开播目录解析：空 dir = 源根（首页源卡播放钮传空路径；语义与
/// `BrowserState.dir = PathBuf::new()` 的根表示一致，各源根由 open_vfs 推导）。
fn resolve_play_dir(req_dir: &Path, root: &Path) -> PathBuf {
    if req_dir.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        req_dir.to_path_buf()
    }
}

/// 【媒体库统计规则（用户定稿 2026-10-21）】媒体库集数统计只计文件夹
/// **当前层**直接包含的视频，不下探子目录——"x 集在看"的分母 = 该文件夹
/// 直接子视频数；子目录内容属于下一级（用户进目录后另行组队/统计）。
/// 递归扫描（collect_videos_recursive）专用于媒体索引，勿混用。
/// 返回 (归一化 path, 文件名, size)；list_dir 失败返回空（统计尽力而为）。
pub fn collect_videos_flat(vfs: &dyn VfsSource, dir: &Path) -> Vec<(String, String, u64)> {
    let Ok(entries) = vfs.list_dir(dir) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| !e.is_dir && is_video_file(&e.path))
        .map(|e| {
            (
                e.path.to_string_lossy().replace('\\', "/"),
                e.name,
                e.size,
            )
        })
        .collect()
}

/// 媒体索引递归收集：目录下**全部子层级**视频（归一化 path/文件名/size）。
/// 深度上限防病态层级（本地 junction/symlink 环）；list_dir 失败的子树
/// 静默跳过（索引尽力而为，下轮每日刷新自愈）。
pub fn collect_videos_recursive(
    vfs: &dyn VfsSource,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(String, String, u64)>,
) {
    const MAX_DEPTH: usize = 12;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = vfs.list_dir(dir) else {
        return;
    };
    for e in entries {
        if e.is_dir {
            collect_videos_recursive(vfs, &e.path, depth + 1, out);
        } else if is_video_file(&e.path) {
            out.push((
                e.path.to_string_lossy().replace('\\', "/"),
                e.name,
                e.size,
            ));
        }
    }
}

/// 媒体库集数统计：**只列文件夹当前层**视频（规则见 collect_videos_flat）
/// + join play_history（只计库内仍存在的媒体，陈旧记录不计入）。
///
/// VFS/DB 失败返回 default（UI 显示"未观看"，下轮历史变化触发重统计）。
pub fn compute_episode_stats(
    sources: &[MediaSource],
    store: &std::sync::Arc<Store>,
    source_id: i64,
    folder: &str,
) -> crate::browse_ui::EpisodeStats {
    let Some(source) = sources.iter().find(|s| s.id == source_id) else {
        return crate::browse_ui::EpisodeStats::default();
    };
    let Ok((vfs, root)) = source.open_vfs() else {
        return crate::browse_ui::EpisodeStats::default();
    };
    // 单视频条目：库即该文件；文件夹条目：只取当前层（不递归）。
    let videos: Vec<String> = if is_video_file(Path::new(folder)) {
        vec![folder.replace('\\', "/")]
    } else {
        let start = if folder.is_empty() {
            root
        } else {
            PathBuf::from(folder)
        };
        collect_videos_flat(&*vfs, &start)
            .into_iter()
            .map(|(p, _, _)| p)
            .collect()
    };
    let records = store.positions_under(source_id, folder).unwrap_or_default();
    join_episode_stats(&videos, records)
}

/// 为单个源重建媒体索引（递归全层级）。能连通（open_vfs 成功）才重建；
/// 连不通返回 None（**保留旧索引**——连接失败不代表媒体消失），调用方
/// warn 跳过。返回索引条数。
pub fn reindex_source(source: &MediaSource, store: &std::sync::Arc<Store>) -> Option<usize> {
    let (vfs, root) = source.open_vfs().ok()?;
    let mut out = Vec::new();
    collect_videos_recursive(&*vfs, &root, 0, &mut out);
    store.replace_media_index(source.id, &out).ok()?;
    Some(out.len())
}

/// 库内视频集合 × 播放记录的 join：只计库内仍存在的媒体（陈旧记录
/// 不计入），分隔符归一 `/`。finished/watching/在看进度和 → EpisodeStats。
pub(crate) fn join_episode_stats(
    videos: &[String],
    records: Vec<(String, f64, f64, bool)>,
) -> crate::browse_ui::EpisodeStats {
    let mut stats = crate::browse_ui::EpisodeStats {
        total: videos.len(),
        ..Default::default()
    };
    for (path, pos, dur, finished) in records {
        let norm = path.replace('\\', "/");
        if !videos.contains(&norm) {
            continue;
        }
        if finished {
            stats.finished += 1;
        } else {
            stats.watching += 1;
            let frac = if dur > 0.0 && pos > 0.0 {
                ((pos / dur) as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            stats.watching_frac_sum += frac;
        }
    }
    stats
}

/// 目录条目排序（目录优先 + 文件名自然序）。拖放播放（drop_play）复用。
pub fn sort_entries(entries: &mut [VfsEntry]) {
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => natural_cmp(&a.name, &b.name),
    });
}

pub fn build_playlist(source_id: i64, entries: &[VfsEntry], clicked: &Path) -> PendingPlay {
    let mut items: Vec<MediaItem> = Vec::new();
    for entry in entries {
        if entry.is_dir || !is_video_file(&entry.path) {
            continue;
        }
        let subs = match_subtitles_for(&entry.path, entries);
        items.push(MediaItem {
            name: entry.name.clone(),
            path: entry.path.clone(),
            size: entry.size,
            subs,
        });
    }
    let start_index = items.iter().position(|i| i.path == clicked).unwrap_or(0);
    PendingPlay {
        source_id,
        items,
        start_index,
        remote: None,
        http_headers: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyplayer_vfs::VfsEntry;

    fn entry(name: &str, is_dir: bool) -> VfsEntry {
        VfsEntry {
            name: name.into(),
            path: PathBuf::from(format!("D:\\media\\{name}")),
            is_dir,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn is_listable_entry_keeps_dirs_videos_subtitles() {
        assert!(is_listable_entry(&entry("Season 1", true)));
        assert!(is_listable_entry(&entry("ep01.mkv", false)));
        assert!(is_listable_entry(&entry("ep01.ass", false)));
        assert!(is_listable_entry(&entry("EP02.MP4", false)));
    }

    #[test]
    fn resolve_play_dir_empty_means_source_root() {
        let root = PathBuf::from("D:\\media");
        assert_eq!(resolve_play_dir(&PathBuf::new(), &root), root);
        let sub = PathBuf::from("D:\\media\\Season 1");
        assert_eq!(resolve_play_dir(&sub, &root), sub);
    }

    #[test]
    fn join_episode_stats_drops_stale_and_normalizes() {
        let videos = vec![
            "D:/A/EP01.mkv".to_string(),
            "D:/A/EP02.mkv".to_string(),
            "D:/A/sub/EP03.mkv".to_string(),
        ];
        let records = vec![
            ("D:/A/EP01.mkv".to_string(), 50.0, 100.0, false),
            // 反斜杠归一后命中。
            ("D:\\A\\EP02.mkv".to_string(), 100.0, 100.0, true),
            // 陈旧记录（文件已不在库内）不计入。
            ("D:/A/gone.mkv".to_string(), 50.0, 100.0, true),
            // 无效时长 → 进度 0。
            ("D:/A/sub/EP03.mkv".to_string(), 10.0, 0.0, false),
        ];
        let stats = join_episode_stats(&videos, records);
        assert_eq!(stats.total, 3);
        assert_eq!(stats.watching, 2);
        assert_eq!(stats.finished, 1);
        assert!((stats.watching_frac_sum - 0.5).abs() < 1e-6);
    }

    /// tempdir 三层结构：root/{EP01.mkv, Season1/{EP02.mkv, Sub/{EP03.mkv}}, note.txt}。
    fn make_tree(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dyplayer-idx-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Season1/Sub")).unwrap();
        std::fs::write(root.join("EP01.mkv"), b"").unwrap();
        std::fs::write(root.join("Season1/EP02.mkv"), b"").unwrap();
        std::fs::write(root.join("Season1/Sub/EP03.mkv"), b"").unwrap();
        std::fs::write(root.join("note.txt"), b"").unwrap();
        root
    }

    #[test]
    fn flat_collects_one_level_only_recursive_all_levels() {
        let root = make_tree("collect");
        let fs = dyplayer_vfs::LocalFs::new(&root).unwrap();
        // LocalFs root 是 canonicalize 后的绝对路径（Windows 带 \\?\ 前缀），
        // VFS 返回的 path 都以它为前缀——断言基准用 fs.root()。
        let canon = fs.root().to_string_lossy().replace('\\', "/");
        // 媒体库统计规则：只取当前层视频（EP01），子目录与 txt 不进列表。
        let flat = collect_videos_flat(&fs, &root);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].1, "EP01.mkv");
        // 媒体索引：递归全层级（3 个视频），路径归一 `/`。
        let mut all = Vec::new();
        collect_videos_recursive(&fs, &root, 0, &mut all);
        let mut names: Vec<&str> = all.iter().map(|(_, n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["EP01.mkv", "EP02.mkv", "EP03.mkv"]);
        assert!(all.iter().all(|(p, _, _)| !p.contains('\\') && p.starts_with(&canon)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn episode_stats_flat_rule_ignores_subfolders() {
        let root = make_tree("stats");
        let store = dyplayer_store::Store::open(&std::path::PathBuf::from(":memory:")).unwrap();
        // 播放记录路径须与 VFS canonical 路径一致（join 按归一字符串匹配）。
        let canon = dyplayer_vfs::LocalFs::new(&root).unwrap().root().to_path_buf();
        // 当前层只有 EP01：total=1（子目录两集不计入媒体库统计口径）。
        store
            .upsert_play_position(1, &canon.join("EP01.mkv").to_string_lossy(), "EP01", 50.0, 100.0, None)
            .unwrap();
        store
            .upsert_play_position(
                1,
                &canon.join("Season1/EP02.mkv").to_string_lossy(),
                "EP02",
                100.0,
                100.0,
                None,
            )
            .unwrap();
        let sources = vec![MediaSource {
            id: 1,
            kind: SourceKind::Local,
            name: "t".into(),
            endpoint: String::new(),
            username: String::new(),
            root: root.clone(),
            accept_invalid_certs: false,
        }];
        let stats = compute_episode_stats(
            &sources,
            &std::sync::Arc::new(store),
            1,
            &canon.to_string_lossy(),
        );
        assert_eq!(stats.total, 1);
        assert_eq!(stats.watching, 1);
        assert_eq!(stats.finished, 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reindex_source_indexes_recursive_and_reports() {
        let root = make_tree("reindex");
        let store = dyplayer_store::Store::open(&std::path::PathBuf::from(":memory:")).unwrap();
        let source = MediaSource {
            id: 42,
            kind: SourceKind::Local,
            name: "t".into(),
            endpoint: String::new(),
            username: String::new(),
            root: root.clone(),
            accept_invalid_certs: false,
        };
        let store = std::sync::Arc::new(store);
        assert_eq!(reindex_source(&source, &store), Some(3));
        assert_eq!(store.media_index_count(42).unwrap(), 3);
        // 子目录条目可被路径段搜到。
        assert_eq!(store.search_media_index(42, "EP03", 10).unwrap().len(), 1);
        assert_eq!(store.search_media_index(42, "Season1", 10).unwrap().len(), 2);
        // 不存在的源（root 无效）：None，旧索引保留。
        let store2 = std::sync::Arc::new(dyplayer_store::Store::open(&std::path::PathBuf::from(":memory:")).unwrap());
        store2.replace_media_index(9, &[("x.mkv".into(), "x.mkv".into(), 1)]).unwrap();
        let bad = MediaSource {
            id: 9,
            root: std::path::PathBuf::from("Z:/definitely/not/exists"),
            ..source
        };
        assert_eq!(reindex_source(&bad, &store2), None);
        assert_eq!(store2.media_index_count(9).unwrap(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn is_listable_entry_drops_unrelated_files() {
        assert!(!is_listable_entry(&entry("cover.jpg", false)));
        assert!(!is_listable_entry(&entry("readme.txt", false)));
        assert!(!is_listable_entry(&entry("movie.nfo", false)));
        assert!(!is_listable_entry(&entry("noext", false)));
    }

    #[test]
    fn source_kind_db_roundtrip() {
        for kind in [SourceKind::Local, SourceKind::WebDav, SourceKind::Smb] {
            assert_eq!(SourceKind::from_db(kind.as_db()), Some(kind));
        }
        assert_eq!(SourceKind::from_db("ftp"), None);
        assert_eq!(SourceKind::WebDav.label(), "WebDAV");
    }

    #[test]
    fn local_source_open_vfs_uses_root() {
        let tmp = std::env::temp_dir().join(format!("dyplayer_msrc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let source = MediaSource {
            id: 1,
            name: "tmp".into(),
            kind: SourceKind::Local,
            endpoint: tmp.display().to_string(),
            username: String::new(),
            root: tmp.clone(),
            accept_invalid_certs: false,
        };
        let (vfs, browse_root) = source.open_vfs().unwrap();
        // LocalFs::new 会 canonicalize（Windows 下带 \\?\ 前缀），比对需同样 canonicalize。
        let expected = tmp.canonicalize().unwrap();
        assert_eq!(browse_root, expected);
        assert!(!source.is_remote());
        let entries = vfs.list_dir(&browse_root).unwrap();
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn remote_source(id: i64, username: &str) -> MediaSource {
        MediaSource {
            id,
            name: "nas".into(),
            kind: SourceKind::WebDav,
            endpoint: "http://127.0.0.1:1/dav".into(),
            username: username.into(),
            root: PathBuf::new(),
            accept_invalid_certs: false,
        }
    }

    /// 填了用户名但 keyring 无凭证：应报"重新填写密码"而不是静默空密码。
    /// 用一个不可能存在凭证的 id 隔离 keyring 状态。
    #[test]
    fn remote_source_missing_credential_reports_guidance() {
        let source = remote_source(990_001, "guest");
        match source.open_vfs() {
            Ok(_) => panic!("无凭证源不应构建成功"),
            Err(e) => assert!(e.contains("重新填写密码"), "got: {e}"),
        }
    }

    /// 匿名源（用户名为空）允许空密码直连，不被 keyring 预检拦截。
    #[test]
    fn anonymous_remote_source_allows_empty_password() {
        let source = remote_source(990_002, "");
        // endpoint 不可达没关系：只要不因 keyring 预检报"重新填写密码"即通过。
        if let Err(e) = source.open_vfs() {
            assert!(!e.contains("重新填写密码"), "got: {e}");
        }
    }

    /// 已存凭证的源：keyring 路径取出后走正常构建（不报"重新填写"）。
    #[test]
    fn stored_credential_source_builds_vfs() {
        let sid = 990_003;
        dyplayer_store::secrets::set_source_credential(sid, "pw-test")
            .expect("keyring set（windows-native feature 缺失时在此暴露）");
        let source = remote_source(sid, "guest");
        match source.open_vfs() {
            Ok((_, root)) => assert_eq!(root, PathBuf::from("/")),
            Err(e) => panic!("已存凭证的源应能构建 VFS: {e}"),
        }
        let _ = dyplayer_store::secrets::delete_source_credential(sid);
    }
}
