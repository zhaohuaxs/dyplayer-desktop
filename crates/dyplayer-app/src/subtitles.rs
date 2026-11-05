use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::bail;

pub fn subtitle_cache_dir() -> anyhow::Result<PathBuf> {
    let dir = dyplayer_store::AppConfig::data_dir()?
        .join("cache")
        .join("subs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn prepare_subtitle(path: &Path, cache_dir: &Path) -> anyhow::Result<PathBuf> {
    let raw = std::fs::read(path)?;
    let is_utf8_like = raw.starts_with(&[0xEF, 0xBB, 0xBF]) || std::str::from_utf8(&raw).is_ok();
    if is_utf8_like {
        return Ok(path.to_path_buf());
    }
    let text = decode_non_utf8(&raw)?;
    std::fs::create_dir_all(cache_dir)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("srt");
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(mtime) = meta.modified()
    {
        mtime.hash(&mut hasher);
    }
    let out = cache_dir.join(format!("{:x}.{ext}", hasher.finish()));
    let mut bytes = Vec::with_capacity(text.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(text.as_bytes());
    std::fs::write(&out, bytes)?;
    Ok(out)
}

/// 远程字幕：下载到的原始字节 → 编码规范化 → 落盘缓存，返回 mpv 可用路径。
/// 与本地 `prepare_subtitle` 的差异：远程没有本地文件，UTF-8 也必须落盘。
pub fn prepare_remote_subtitle(
    vfs_path: &Path,
    raw: &[u8],
    cache_dir: &Path,
) -> anyhow::Result<PathBuf> {
    if raw.is_empty() {
        bail!("字幕内容为空");
    }
    let ext = vfs_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("srt");
    let mut hasher = DefaultHasher::new();
    vfs_path.hash(&mut hasher);
    raw.hash(&mut hasher);
    let out = cache_dir.join(format!("{:x}.{ext}", hasher.finish()));
    if !out.exists() {
        std::fs::create_dir_all(cache_dir)?;
        let text = decode_maybe_utf8(raw)?;
        let mut bytes = Vec::with_capacity(text.len() + 3);
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        bytes.extend_from_slice(text.as_bytes());
        std::fs::write(&out, bytes)?;
    }
    Ok(out)
}

fn decode_maybe_utf8(raw: &[u8]) -> anyhow::Result<String> {
    let is_utf8_like = raw.starts_with(&[0xEF, 0xBB, 0xBF]) || std::str::from_utf8(raw).is_ok();
    if is_utf8_like {
        let text = String::from_utf8_lossy(raw)
            .trim_start_matches('\u{FEFF}')
            .trim()
            .to_string();
        return Ok(text);
    }
    decode_non_utf8(raw)
}

fn decode_non_utf8(raw: &[u8]) -> anyhow::Result<String> {
    if raw.starts_with(&[0xFF, 0xFE]) {
        let (cow, _, _) = encoding_rs::UTF_16LE.decode(&raw[2..]);
        return Ok(cow.into_owned());
    }
    if raw.starts_with(&[0xFE, 0xFF]) {
        let (cow, _, _) = encoding_rs::UTF_16BE.decode(&raw[2..]);
        return Ok(cow.into_owned());
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(raw);
    if had_errors {
        bail!("无法识别字幕编码（既非 UTF-8 也非 GBK/UTF-16）");
    }
    Ok(cow.into_owned())
}

pub fn describe_prepare_failure(path: &Path, e: &anyhow::Error) -> String {
    format!("字幕加载失败 {}: {e}", path.display())
}
