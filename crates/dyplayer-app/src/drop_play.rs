//! 拖放播放：文件分流 / 目录扫描 / 队列构造纯函数层。
//!
//! 规则（见设计定稿）：
//! - 单视频拖入：同目录组队并定位（与媒体库点击视频语义一致）
//! - 多视频拖入：显式集合（仅拖入文件，组内自然序），不扩到全目录
//! - 文件夹拖入：只扫一层（与"播放文件夹"一致），无视频报错
//! - 混合拖入：保持拖入原始顺序，视频块/文件夹块按首次出现位置拼接
//! - XML：取第一个，其余忽略计数
//! - 拖拽播放 source_id 固定为 0（play_history 无外键约束）

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dyplayer_vfs::{
    VfsEntry, VfsSource, extension_of, is_video_file, match_subtitles_for, natural_cmp,
};

use crate::browser::{build_playlist, sort_entries};
use crate::player_app::{MediaItem, PendingPlay};

/// 拖拽播放（临时播放）使用的固定 source_id。
pub const DROP_SOURCE_ID: i64 = 0;

/// 单个拖入项的分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DroppedItem {
    Folder(PathBuf),
    Video(PathBuf),
    Xml(PathBuf),
    Ignored(PathBuf),
}

/// 按类型分流拖入路径（保持原始顺序）。
pub fn classify_dropped(paths: Vec<PathBuf>) -> Vec<DroppedItem> {
    paths
        .into_iter()
        .map(|p| {
            if p.is_dir() {
                DroppedItem::Folder(p)
            } else if is_video_file(&p) {
                DroppedItem::Video(p)
            } else if extension_of(&p).as_deref() == Some("xml") {
                DroppedItem::Xml(p)
            } else {
                DroppedItem::Ignored(p)
            }
        })
        .collect()
}

/// 拖放组队结果。
#[derive(Debug, Default)]
pub struct DropPlayPlan {
    /// 待播放队列（视频/文件夹存在且组队成功时）。
    pub pending: Option<PendingPlay>,
    /// 第一个 XML 弹幕文件。
    pub xml: Option<PathBuf>,
    /// 被忽略的额外 XML 数量。
    pub extra_xmls: usize,
    /// 不支持的文件。
    pub ignored: Vec<PathBuf>,
    /// 组队过程中的错误（文件夹无视频/扫描失败等）。
    pub errors: Vec<String>,
}

/// 由分类结果构造播放计划。
pub fn build_drop_playlist(items: &[DroppedItem]) -> DropPlayPlan {
    let mut plan = DropPlayPlan::default();
    let mut videos: Vec<PathBuf> = Vec::new();
    let mut has_folder = false;
    for item in items {
        match item {
            DroppedItem::Xml(p) => {
                if plan.xml.is_none() {
                    plan.xml = Some(p.clone());
                } else {
                    plan.extra_xmls += 1;
                }
            }
            DroppedItem::Ignored(p) => plan.ignored.push(p.clone()),
            DroppedItem::Video(p) => videos.push(p.clone()),
            DroppedItem::Folder(_) => has_folder = true,
        }
    }

    if !has_folder && videos.len() == 1 {
        match playlist_for_single_video(&videos[0]) {
            Ok(pending) => plan.pending = Some(pending),
            Err(e) => plan.errors.push(e),
        }
        return plan;
    }

    if videos.is_empty() && !has_folder {
        return plan;
    }

    let queue = build_queue_blocks(items, &mut plan.errors);
    if !queue.is_empty() {
        plan.pending = Some(PendingPlay {
            source_id: DROP_SOURCE_ID,
            items: queue,
            start_index: 0,
            remote: None,
            http_headers: None,
        });
    }
    plan
}

/// 单视频拖入：扫同目录组队并定位到该视频（与媒体库点击一致）。
/// 拖入路径先 canonicalize，与 LocalFs 扫描产物（含 `\\?\` 前缀）形态对齐，
/// 否则 build_playlist 的路径全等定位会失败。
pub fn playlist_for_single_video(video: &Path) -> Result<PendingPlay, String> {
    let canonical =
        std::fs::canonicalize(video).map_err(|e| format!("无法访问 {}: {e}", video.display()))?;
    let parent = canonical
        .parent()
        .ok_or_else(|| "无法定位父目录".to_string())?
        .to_path_buf();
    let entries = scan_dir_entries(&parent).map_err(|e| e.to_string())?;
    let pending = build_playlist(DROP_SOURCE_ID, &entries, &canonical);
    if pending.items.is_empty() {
        return Err("目录内没有视频文件".into());
    }
    Ok(pending)
}

/// 扫描目录一层（排序后返回全部条目，供组队与字幕匹配复用）。
fn scan_dir_entries(dir: &Path) -> dyplayer_vfs::VfsResult<Vec<VfsEntry>> {
    let fs = dyplayer_vfs::LocalFs::new(dir)?;
    let mut entries = fs.list_dir(dir)?;
    sort_entries(&mut entries);
    Ok(entries)
}

/// 组队块：相邻显式视频合并为一块，文件夹独立成块，保持首次出现顺序。
enum QueueBlock {
    Videos(Vec<PathBuf>),
    Folder(PathBuf),
}

fn build_queue_blocks(items: &[DroppedItem], errors: &mut Vec<String>) -> Vec<MediaItem> {
    let mut blocks: Vec<QueueBlock> = Vec::new();
    for item in items {
        match item {
            DroppedItem::Video(p) => match blocks.last_mut() {
                Some(QueueBlock::Videos(v)) => v.push(p.clone()),
                _ => blocks.push(QueueBlock::Videos(vec![p.clone()])),
            },
            DroppedItem::Folder(p) => blocks.push(QueueBlock::Folder(p.clone())),
            _ => {}
        }
    }

    let mut queue: Vec<MediaItem> = Vec::new();
    let mut dir_cache: HashMap<PathBuf, Vec<VfsEntry>> = HashMap::new();
    for block in blocks {
        match block {
            QueueBlock::Videos(paths) => {
                let mut canon: Vec<PathBuf> = Vec::with_capacity(paths.len());
                for p in &paths {
                    match std::fs::canonicalize(p) {
                        Ok(c) => canon.push(c),
                        Err(e) => errors.push(format!("无法访问 {}: {e}", p.display())),
                    }
                }
                canon.sort_by(|a, b| natural_cmp(&file_name_of(a), &file_name_of(b)));
                for path in canon {
                    queue.push(media_item_for(&path, &mut dir_cache));
                }
            }
            QueueBlock::Folder(dir) => match scan_dir_entries(&dir) {
                Ok(entries) => {
                    let has_video = entries.iter().any(|e| !e.is_dir && is_video_file(&e.path));
                    if !has_video {
                        errors.push(format!("{}：文件夹内没有视频文件", dir.display()));
                        continue;
                    }
                    for entry in &entries {
                        if entry.is_dir || !is_video_file(&entry.path) {
                            continue;
                        }
                        let subs = match_subtitles_for(&entry.path, &entries);
                        queue.push(MediaItem {
                            name: entry.name.clone(),
                            path: entry.path.clone(),
                            size: entry.size,
                            subs,
                        });
                    }
                }
                Err(e) => errors.push(format!("扫描 {} 失败: {e}", dir.display())),
            },
        }
    }
    queue
}

fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 显式视频项：按父目录缓存条目，挂载同名字幕候选。
fn media_item_for(path: &Path, dir_cache: &mut HashMap<PathBuf, Vec<VfsEntry>>) -> MediaItem {
    let name = {
        let n = file_name_of(path);
        if n.is_empty() {
            path.display().to_string()
        } else {
            n
        }
    };
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let entries: &Vec<VfsEntry> = dir_cache
        .entry(parent.clone())
        .or_insert_with(|| scan_dir_entries(&parent).unwrap_or_default());
    let subs = match_subtitles_for(path, entries);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    MediaItem {
        name,
        path: path.to_path_buf(),
        size,
        subs,
    }
}

/// 多视频显式集合的组内自然序测试（ep2 < ep10）。
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dyplayer_drop_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn classify_sorts_folders_videos_xmls_ignored() {
        let root = temp_root("classify");
        let dir = root.join("season");
        fs::create_dir_all(&dir).unwrap();
        let video = touch(&root, "a.mp4");
        let xml = touch(&root, "b.xml");
        let txt = touch(&root, "c.txt");

        let items = classify_dropped(vec![dir.clone(), video.clone(), xml.clone(), txt.clone()]);
        assert_eq!(
            items,
            vec![
                DroppedItem::Folder(dir),
                DroppedItem::Video(video),
                DroppedItem::Xml(xml),
                DroppedItem::Ignored(txt),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_xml_is_case_insensitive() {
        let root = temp_root("xmlcase");
        let xml = touch(&root, "danmaku.XML");
        let items = classify_dropped(vec![xml.clone()]);
        assert_eq!(items, vec![DroppedItem::Xml(xml)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn single_video_builds_folder_playlist_and_locates() {
        let root = temp_root("single");
        touch(&root, "ep01.mp4");
        let ep02 = touch(&root, "ep02.mp4");
        touch(&root, "ep02.ass");

        let pending = playlist_for_single_video(&ep02).unwrap();
        assert_eq!(pending.items.len(), 2);
        assert_eq!(pending.start_index, 1);
        assert_eq!(pending.source_id, DROP_SOURCE_ID);
        assert!(pending.items[1].name.starts_with("ep02"));
        assert_eq!(pending.items[1].subs.len(), 1);
        assert!(pending.items[1].subs[0].exact);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn single_video_missing_file_reports_error() {
        let root = temp_root("single_missing");
        let missing = root.join("nope.mp4");
        let err = playlist_for_single_video(&missing).unwrap_err();
        assert!(err.contains("无法访问"), "got: {err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn single_video_via_plan_uses_folder_playlist() {
        let root = temp_root("single_plan");
        touch(&root, "ep01.mp4");
        let ep02 = touch(&root, "ep02.mp4");

        let items = classify_dropped(vec![ep02]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        // 同目录组队：ep01 也在队列里，定位到 ep02
        assert_eq!(pending.items.len(), 2);
        assert_eq!(pending.start_index, 1);
        assert!(plan.errors.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn multiple_videos_form_explicit_set() {
        let root = temp_root("multi");
        touch(&root, "ep01.mp4");
        let ep02 = touch(&root, "ep02.mp4");
        let ep10 = touch(&root, "ep10.mp4");
        touch(&root, "ep02.chs.srt");

        let items = classify_dropped(vec![ep10.clone(), ep02.clone()]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        // 显式集合：仅拖入的 2 个，自然序 ep02 在前
        assert_eq!(pending.items.len(), 2);
        assert_eq!(pending.start_index, 0);
        assert!(pending.items[0].name.starts_with("ep02"));
        assert!(pending.items[1].name.starts_with("ep10"));
        // 字幕候选仍从同目录挂载
        assert_eq!(pending.items[0].subs.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_videos_sort_naturally() {
        // ep2 < ep10：自然序而非字典序
        let root = temp_root("naturalsort");
        let ep2 = touch(&root, "ep2.mp4");
        let ep10 = touch(&root, "ep10.mp4");

        let items = classify_dropped(vec![ep10, ep2]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        assert!(pending.items[0].name.starts_with("ep2"));
        assert!(pending.items[1].name.starts_with("ep10"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_folder_reports_no_video() {
        let root = temp_root("empty_folder");
        let empty = root.join("nothing");
        fs::create_dir_all(&empty).unwrap();

        let items = classify_dropped(vec![empty.clone()]);
        let plan = build_drop_playlist(&items);
        assert!(plan.pending.is_none());
        assert_eq!(plan.errors.len(), 1);
        assert!(
            plan.errors[0].contains("没有视频"),
            "got: {}",
            plan.errors[0]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folder_expands_videos_in_natural_order_with_subs() {
        let root = temp_root("folder");
        let season = root.join("season");
        fs::create_dir_all(&season).unwrap();
        touch(&season, "ep10.mp4");
        touch(&season, "ep2.mp4");
        touch(&season, "ep2.srt");
        touch(&season, "readme.txt");

        let items = classify_dropped(vec![season]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        assert_eq!(pending.items.len(), 2);
        assert!(pending.items[0].name.starts_with("ep2"));
        assert!(pending.items[1].name.starts_with("ep10"));
        assert_eq!(pending.items[0].subs.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mixed_keeps_drop_order_blocks() {
        let root = temp_root("mixed");
        let season = root.join("season");
        fs::create_dir_all(&season).unwrap();
        let single = touch(&root, "z_single.mp4");
        touch(&season, "a1.mp4");
        touch(&season, "a2.mp4");

        // 先拖单个视频，再拖文件夹 → [z_single, a1, a2]
        let items = classify_dropped(vec![single.clone(), season.clone()]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        assert_eq!(pending.items.len(), 3);
        assert!(pending.items[0].name.starts_with("z_single"));
        assert!(pending.items[1].name.starts_with("a1"));
        assert!(pending.items[2].name.starts_with("a2"));

        // 反向：先文件夹后视频 → [a1, a2, z_single]
        let items = classify_dropped(vec![season, single]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        assert!(pending.items[0].name.starts_with("a1"));
        assert!(pending.items[2].name.starts_with("z_single"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn video_with_folder_uses_explicit_semantics() {
        // 混合时视频不再同目录组队（避免与文件夹内容重复），走显式项
        let root = temp_root("mixvideo");
        let season = root.join("season");
        fs::create_dir_all(&season).unwrap();
        let outside = touch(&root, "outside.mp4");
        touch(&season, "inside.mp4");
        touch(&root, "sibling.mp4");

        let items = classify_dropped(vec![outside, season]);
        let plan = build_drop_playlist(&items);
        let pending = plan.pending.unwrap();
        // 只有显式拖入的 outside + 文件夹内的 inside；sibling 不进队列
        assert_eq!(pending.items.len(), 2);
        assert!(pending.items[0].name.starts_with("outside"));
        assert!(pending.items[1].name.starts_with("inside"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn multiple_xmls_keep_first_count_rest() {
        let root = temp_root("xmlmulti");
        let xml1 = touch(&root, "d1.xml");
        let xml2 = touch(&root, "d2.xml");

        let items = classify_dropped(vec![xml1.clone(), xml2]);
        let plan = build_drop_playlist(&items);
        assert_eq!(plan.xml, Some(xml1));
        assert_eq!(plan.extra_xmls, 1);
        assert!(plan.pending.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ignored_files_collected_without_pending() {
        let root = temp_root("ignored");
        let txt = touch(&root, "note.txt");

        let items = classify_dropped(vec![txt.clone()]);
        let plan = build_drop_playlist(&items);
        assert!(plan.pending.is_none());
        assert_eq!(plan.ignored, vec![txt]);
        assert!(plan.errors.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn video_plus_xml_plan_carries_both() {
        let root = temp_root("videoxml");
        let video = touch(&root, "ep01.mp4");
        let xml = touch(&root, "other.xml");
        touch(&root, "junk.txt");

        let items = classify_dropped(vec![video, xml.clone(), root.join("junk.txt")]);
        let plan = build_drop_playlist(&items);
        assert!(plan.pending.is_some());
        assert_eq!(plan.xml, Some(xml));
        assert_eq!(plan.ignored.len(), 1);
        assert!(plan.errors.is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
