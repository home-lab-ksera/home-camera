use std::collections::HashMap;

use home_camera::config::AppConfig;
use proptest::prelude::*;

fn base_env() -> HashMap<String, String> {
    HashMap::from([
        (
            "RTSP_URL".to_string(),
            "rtsp://camera.local/live".to_string(),
        ),
        (
            "API_HUB_BASE_URL".to_string(),
            "http://api-hub.sandbox.svc:3000".to_string(),
        ),
    ])
}

proptest! {
    #[test]
    fn ffmpeg_crf_in_range_is_accepted(crf in 0u8..=51u8) {
        let mut vars = base_env();
        vars.insert("FFMPEG_CRF".to_string(), crf.to_string());

        let cfg = AppConfig::from_map(&vars).expect("config should parse");
        prop_assert_eq!(cfg.ffmpeg_crf, crf);
    }
}

#[test]
fn missing_required_fields_fail() {
    let required = ["RTSP_URL", "API_HUB_BASE_URL"];

    for key in required {
        let mut vars = base_env();
        vars.remove(key);
        assert!(
            AppConfig::from_map(&vars).is_err(),
            "{key} should be required"
        );
    }
}

#[test]
fn ffmpeg_defaults_are_applied() {
    let vars = base_env();
    let cfg = AppConfig::from_map(&vars).expect("config should parse");
    assert_eq!(
        cfg.ffmpeg_video_codec,
        AppConfig::DEFAULT_FFMPEG_VIDEO_CODEC
    );
    assert_eq!(
        cfg.ffmpeg_audio_codec,
        AppConfig::DEFAULT_FFMPEG_AUDIO_CODEC
    );
    assert_eq!(
        cfg.ffmpeg_audio_bitrate,
        AppConfig::DEFAULT_FFMPEG_AUDIO_BITRATE
    );
    assert_eq!(cfg.ffmpeg_preset, AppConfig::DEFAULT_FFMPEG_PRESET);
    assert_eq!(cfg.ffmpeg_crf, AppConfig::DEFAULT_FFMPEG_CRF);
}

#[test]
fn ffmpeg_crf_above_max_is_rejected() {
    let mut vars = base_env();
    vars.insert("FFMPEG_CRF".to_string(), "52".to_string());
    assert!(AppConfig::from_map(&vars).is_err());
}

#[test]
fn empty_new_ffmpeg_settings_are_rejected() {
    for key in [
        "FFMPEG_VIDEO_CODEC",
        "FFMPEG_AUDIO_CODEC",
        "FFMPEG_AUDIO_BITRATE",
    ] {
        let mut vars = base_env();
        vars.insert(key.to_string(), "   ".to_string());
        assert!(AppConfig::from_map(&vars).is_err(), "{key} should fail");
    }
}

#[test]
fn invalid_api_hub_urls_are_rejected() {
    for url in [
        "",
        "   ",
        "file:///tmp/api",
        "http://",
        "http://api-hub?key=x",
        "http://api-hub#fragment",
    ] {
        let mut vars = base_env();
        vars.insert("API_HUB_BASE_URL".to_string(), url.to_string());
        assert!(AppConfig::from_map(&vars).is_err(), "{url} should fail");
    }
}
