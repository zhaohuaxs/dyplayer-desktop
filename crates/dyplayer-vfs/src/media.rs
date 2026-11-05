use std::cmp::Ordering;
use std::path::Path;

pub(crate) const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "flv", "ts", "m2ts", "mpg", "mpeg", "rm", "rmvb", "wmv",
    "m4v", "iso", "ogv", "3gp",
];

pub(crate) const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "sup", "ttml"];

pub fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

pub fn is_video_file(path: &Path) -> bool {
    extension_of(path).is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.as_str()))
}

pub fn is_subtitle_file(path: &Path) -> bool {
    extension_of(path).is_some_and(|ext| SUBTITLE_EXTENSIONS.contains(&ext.as_str()))
}

pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natord::compare_ignore_case(a, b)
}

const LANGUAGE_SUFFIXES: &[(&str, &str)] = &[
    ("chs", "zh-Hans"),
    ("sc", "zh-Hans"),
    ("zh-hans", "zh-Hans"),
    ("gb", "zh-Hans"),
    ("cht", "zh-Hant"),
    ("tc", "zh-Hant"),
    ("zh-hant", "zh-Hant"),
    ("big5", "zh-Hant"),
    ("zh", "zh"),
    ("cn", "zh-Hans"),
    ("zh-cn", "zh-Hans"),
    ("zh-tw", "zh-Hant"),
    ("en", "en"),
    ("eng", "en"),
    ("jp", "ja"),
    ("jpn", "ja"),
    ("ja", "ja"),
];

#[derive(Debug, Clone)]
pub struct SubtitleCandidate {
    pub path: std::path::PathBuf,
    pub title: String,
    pub exact: bool,
    pub lang: Option<String>,
}

fn language_of(stem_suffix: &str) -> Option<String> {
    let lower = stem_suffix.to_ascii_lowercase();
    for (suffix, lang) in LANGUAGE_SUFFIXES {
        if lower == *suffix {
            return Some((*lang).to_string());
        }
    }
    None
}

pub fn match_subtitles_for(
    video_path: &Path,
    dir_entries: &[crate::types::VfsEntry],
) -> Vec<SubtitleCandidate> {
    let Some(video_stem) = video_path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let mut exact = Vec::new();
    let mut prefixed = Vec::new();
    for entry in dir_entries {
        if entry.is_dir || !is_subtitle_file(&entry.path) {
            continue;
        }
        let Some(stem) = entry.path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == video_stem {
            exact.push(entry.clone());
        } else if stem.len() > video_stem.len() && stem.starts_with(video_stem) {
            let sep = stem.as_bytes()[video_stem.len()];
            if sep == b'.' || sep == b' ' || sep == b'_' || sep == b'-' {
                prefixed.push(entry.clone());
            }
        }
    }
    let to_candidate = |entry: &crate::types::VfsEntry, is_exact: bool| SubtitleCandidate {
        title: entry.name.clone(),
        path: entry.path.clone(),
        exact: is_exact,
        lang: entry
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|stem| {
                stem[video_stem.len().min(stem.len())..].trim_start_matches(['.', ' ', '_', '-'])
            })
            .and_then(language_of),
    };
    let mut candidates: Vec<SubtitleCandidate> =
        exact.iter().map(|e| to_candidate(e, true)).collect();
    let mut prefix_candidates: Vec<SubtitleCandidate> =
        prefixed.iter().map(|e| to_candidate(e, false)).collect();
    prefix_candidates.sort_by(|a, b| natural_cmp(&a.title, &b.title));
    candidates.extend(prefix_candidates);
    candidates
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VfsEntry;
    use std::path::PathBuf;

    fn entry(name: &str) -> VfsEntry {
        VfsEntry {
            name: name.into(),
            path: PathBuf::from(name),
            is_dir: false,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn extension_detection_ignores_case() {
        assert!(is_video_file(Path::new("A.MP4")));
        assert!(is_video_file(Path::new("b.mkv")));
        assert!(!is_video_file(Path::new("c.txt")));
        assert!(!is_video_file(Path::new("noext")));
        assert!(is_subtitle_file(Path::new("D.Srt")));
        assert!(!is_subtitle_file(Path::new("e.doc")));
    }

    #[test]
    fn natural_cmp_sorts_numbers() {
        assert_eq!(natural_cmp("EP2", "EP10"), Ordering::Less);
        assert_eq!(natural_cmp("EP10", "EP2"), Ordering::Greater);
        assert_eq!(natural_cmp("EP2", "EP2"), Ordering::Equal);
    }

    #[test]
    fn subtitle_matching_prefers_exact_then_prefixed() {
        let entries = vec![
            entry("show.srt"),
            entry("show.chs.srt"),
            entry("show-en.srt"),
            entry("showX.srt"),
            entry("other.srt"),
            VfsEntry {
                name: "subdir".into(),
                path: PathBuf::from("subdir"),
                is_dir: true,
                size: 0,
                modified: None,
            },
        ];
        let cands = match_subtitles_for(Path::new("show.mp4"), &entries);
        assert_eq!(cands.len(), 3);
        assert!(cands[0].exact);
        assert_eq!(cands[0].title, "show.srt");
        let prefixed_titles: Vec<&str> = cands[1..].iter().map(|c| c.title.as_str()).collect();
        assert!(prefixed_titles.contains(&"show.chs.srt"));
        assert!(prefixed_titles.contains(&"show-en.srt"));
        assert!(!prefixed_titles.contains(&"showX.srt"));
    }

    #[test]
    fn subtitle_language_detection() {
        let entries = vec![
            entry("show.srt"),
            entry("show.chs.srt"),
            entry("show.zh.srt"),
            entry("show.abc.srt"),
        ];
        let cands = match_subtitles_for(Path::new("show.mp4"), &entries);
        let langs: Vec<Option<String>> = cands.iter().map(|c| c.lang.clone()).collect();
        assert!(langs.contains(&Some("zh-Hans".into())));
        assert!(langs.contains(&Some("zh".into())));
        assert!(langs.contains(&None));
    }
}
