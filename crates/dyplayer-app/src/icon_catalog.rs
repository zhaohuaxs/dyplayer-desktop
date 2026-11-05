//! modernz-icons.ttf 未使用图标目录（静态数据）。
//!
//! 数据由字体 GSUB ligature 名解析生成（post 3.0 无真名），已排除
//! osc.rs `icons` 模块使用的 21 个码点。供 browse_ui 设置页"开发"组的
//! 图标目录浏览器弹窗列举，方便后续查询/复用。
//!
//! 再生成：`python dyplayer/tmp/gen_icon_catalog.py`
//! （解析逻辑见同目录 icon_usage_diag.py）

/// Material 系图标（55 个）。
pub const MATERIAL: &[(char, &str)] = &[
    ('\u{F000}', "material_zoom_out"),
    ('\u{F001}', "material_zoom_in"),
    ('\u{F002}', "material_volume_up_filled"),
    ('\u{F003}', "material_volume_up"),
    ('\u{F004}', "material_volume_off_filled"),
    ('\u{F005}', "material_volume_off"),
    ('\u{F006}', "material_volume_mute_filled"),
    ('\u{F007}', "material_volume_mute"),
    ('\u{F008}', "material_volume_downfilled"),
    ('\u{F009}', "material_volume_down"),
    ('\u{F00A}', "material_surround_sound_filled"),
    ('\u{F00B}', "material_surround_sound"),
    ('\u{F00C}', "material_subtitles_filled"),
    ('\u{F00D}', "material_subtitles"),
    ('\u{F00E}', "material_speed_filled"),
    ('\u{F00F}', "material_speed"),
    ('\u{F010}', "material_skip_previous_filled"),
    ('\u{F011}', "material_skip_next_filled"),
    ('\u{F012}', "material_shuffle_on"),
    ('\u{F013}', "material_shuffle"),
    ('\u{F014}', "material_replay_30"),
    ('\u{F015}', "material_replay_10"),
    ('\u{F016}', "material_replay_5"),
    ('\u{F018}', "material_repeat_one"),
    ('\u{F019}', "material_repeat_on"),
    ('\u{F01A}', "material_repeat"),
    ('\u{F01B}', "material_playlist_play"),
    ('\u{F01C}', "material_play_arrow_filled"),
    ('\u{F01D}', "material_play_arrow"),
    ('\u{F01E}', "material_pip_exit_filled"),
    ('\u{F01F}', "material_pip_exit"),
    ('\u{F020}', "material_pip"),
    ('\u{F021}', "material_photo_camera_filled"),
    ('\u{F022}', "material_photo_camera"),
    ('\u{F023}', "material_pause_filled"),
    ('\u{F024}', "material_pause"),
    ('\u{F025}', "material_no_sound_filled"),
    ('\u{F026}', "material_no_sound"),
    ('\u{F027}', "material_more_vert"),
    ('\u{F028}', "material_info_filled"),
    ('\u{F029}', "material_info"),
    ('\u{F02A}', "material_fullscreen_exit"),
    ('\u{F02B}', "material_fullscreen"),
    ('\u{F02C}', "material_forward_30"),
    ('\u{F02D}', "material_forward_10"),
    ('\u{F02E}', "material_forward_5"),
    ('\u{F02F}', "material_skip_forward"),
    ('\u{F030}', "material_fast_rewind_filled"),
    ('\u{F031}', "material_fast_forward_filled"),
    ('\u{F032}', "material_downloading"),
    ('\u{F033}', "material_download"),
    ('\u{F034}', "material_skip_previous"),
    ('\u{F035}', "material_skip_next"),
    ('\u{F036}', "material_fast_rewind"),
    ('\u{F037}', "material_fast_forward"),
];

/// Fluent 系图标（66 个）。
pub const FLUENT: &[(char, &str)] = &[
    ('\u{F03B}', "fluent_zoom_out_filled"),
    ('\u{F03C}', "fluent_zoom_out"),
    ('\u{F03D}', "fluent_zoom_in_filled"),
    ('\u{F03E}', "fluent_zoom_in"),
    ('\u{F03F}', "fluent_speed_filled"),
    ('\u{F040}', "fluent_speed"),
    ('\u{F041}', "fluent_subtitles_filled"),
    ('\u{F043}', "fluent_volume_off_filled"),
    ('\u{F044}', "fluent_volume_off"),
    ('\u{F045}', "fluent_no_sound_filled"),
    ('\u{F047}', "fluent_volume_up_filled"),
    ('\u{F049}', "fluent_volume_down_filled"),
    ('\u{F04A}', "fluent_volume_down"),
    ('\u{F04B}', "fluent_volume_mute_filled"),
    ('\u{F04D}', "fluent_skip_forward_filled"),
    ('\u{F04E}', "fluent_skip_forward"),
    ('\u{F04F}', "fluent_forward_30_filled"),
    ('\u{F050}', "fluent_forward_30"),
    ('\u{F052}', "fluent_forward_10"),
    ('\u{F053}', "fluent_forward_5_filled"),
    ('\u{F054}', "fluent_forward_5"),
    ('\u{F055}', "fluent_skip_back_filled"),
    ('\u{F056}', "fluent_skip_back"),
    ('\u{F057}', "fluent_replay_30_filled"),
    ('\u{F058}', "fluent_replay_30"),
    ('\u{F05A}', "fluent_replay_10"),
    ('\u{F05B}', "fluent_replay_5_filled"),
    ('\u{F05C}', "fluent_replay_5"),
    ('\u{F05D}', "fluent_fast_rewind_filled"),
    ('\u{F05E}', "fluent_fast_rewind"),
    ('\u{F05F}', "fluent_replay_filled"),
    ('\u{F060}', "fluent_replay"),
    ('\u{F062}', "fluent_skip_previous"),
    ('\u{F063}', "fluent_playlist_play_filled"),
    ('\u{F066}', "fluent_play_arrow"),
    ('\u{F067}', "fluent_pip_alt_filled"),
    ('\u{F068}', "fluent_pip_exit_filled_alt"),
    ('\u{F069}', "fluent_pip_exit_alt"),
    ('\u{F06A}', "fluent_pip_filled"),
    ('\u{F06B}', "fluent_pip"),
    ('\u{F06C}', "fluent_pip_alt"),
    ('\u{F06E}', "fluent_pause"),
    ('\u{F070}', "fluent_skip_next"),
    ('\u{F071}', "fluent_more_vert_filled"),
    ('\u{F072}', "fluent_more_vert"),
    ('\u{F073}', "fluent_info_filled"),
    ('\u{F075}', "fluent_fast_forward_filled"),
    ('\u{F076}', "fluent_fast_forward"),
    ('\u{F077}', "fluent_downloading_filled"),
    ('\u{F078}', "fluent_downloading"),
    ('\u{F079}', "fluent_photo_camera_filled"),
    ('\u{F07B}', "fluent_surround_sound_filled"),
    ('\u{F07D}', "fluent_shuffle_filled"),
    ('\u{F07E}', "fluent_shuffle"),
    ('\u{F07F}', "fluent_shuffle_on_filled"),
    ('\u{F080}', "fluent_shuffle_on"),
    ('\u{F081}', "fluent_repeat_filled"),
    ('\u{F083}', "fluent_repeat_on_filled"),
    ('\u{F085}', "fluent_repeat_one_filled"),
    ('\u{F086}', "fluent_repeat_one"),
    ('\u{F087}', "fluent_fullscreen_exit_filled"),
    ('\u{F089}', "fluent_fullscreen_filled"),
    ('\u{F08B}', "fluent_download_filled"),
    ('\u{F08C}', "fluent_download"),
    ('\u{F08E}', "fluent_pip_exit_filled"),
    ('\u{F08F}', "fluent_pip_exit"),
];

/// 无系列前缀图标（3 个）。
pub const OTHER: &[(char, &str)] = &[
    ('\u{F017}', "skip_back"),
    ('\u{F08D}', "window_unmaximize"),
    ('\u{F090}', "warning"),
];

/// 全部未使用图标（三组拼接）。
pub fn all() -> impl Iterator<Item = (char, &'static str)> {
    MATERIAL.iter().chain(FLUENT).chain(OTHER).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_counts_match_font() {
        assert_eq!(MATERIAL.len(), 55);
        assert_eq!(FLUENT.len(), 66);
        assert_eq!(OTHER.len(), 3);
        assert_eq!(all().count(), 124);
    }

    #[test]
    fn codepoints_unique_and_pua() {
        let mut seen = std::collections::HashSet::new();
        for (cp, _) in all() {
            assert!((0xE000..=0xF8FF).contains(&(cp as u32)), "{cp:?} not PUA");
            assert!(seen.insert(cp), "duplicate codepoint {cp:?}");
        }
    }

    #[test]
    fn names_nonempty() {
        assert!(all().all(|(_, name)| !name.is_empty() && name.is_ascii()));
    }
}
