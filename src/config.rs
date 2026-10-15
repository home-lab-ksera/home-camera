use std::collections::HashMap;

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub rtsp_url: String,
    pub api_hub_base_url: String,
    pub s3_bucket: String,
    pub camera_id: String,
    pub record_seconds: u64,
    pub ffmpeg_video_codec: String,
    pub ffmpeg_audio_codec: String,
    pub ffmpeg_audio_bitrate: String,
    pub ffmpeg_loglevel: String,
    pub ffmpeg_preset: String,
    pub ffmpeg_crf: u8,
}

impl AppConfig {
    pub const DEFAULT_BUCKET: &'static str = "home-camera-recordings";
    pub const DEFAULT_CAMERA_ID: &'static str = "camera";
    pub const DEFAULT_RECORD_SECONDS: u64 = 3600;
    pub const DEFAULT_FFMPEG_VIDEO_CODEC: &'static str = "copy";
    pub const DEFAULT_FFMPEG_AUDIO_CODEC: &'static str = "copy";
    pub const DEFAULT_FFMPEG_AUDIO_BITRATE: &'static str = "128k";
    pub const DEFAULT_FFMPEG_LOGLEVEL: &'static str = "warning";
    pub const DEFAULT_FFMPEG_PRESET: &'static str = "veryfast";
    pub const DEFAULT_FFMPEG_CRF: u8 = 23;

    pub fn from_env() -> Result<Self, AppError> {
        let mut map = HashMap::new();
        for (k, v) in std::env::vars() {
            map.insert(k, v);
        }
        Self::from_map(&map)
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, AppError> {
        let rtsp_url = required(vars, "RTSP_URL")?;
        if !rtsp_url.starts_with("rtsp://") {
            return Err(AppError::InvalidEnv {
                name: "RTSP_URL",
                reason: "must start with rtsp://".to_string(),
            });
        }

        let api_hub_base_url = required(vars, "API_HUB_BASE_URL")?;
        let url = reqwest::Url::parse(&api_hub_base_url).map_err(|e| AppError::InvalidEnv {
            name: "API_HUB_BASE_URL",
            reason: e.to_string(),
        })?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AppError::InvalidEnv {
                name: "API_HUB_BASE_URL",
                reason: "must be an HTTP(S) base URL without query or fragment".to_string(),
            });
        }
        let api_hub_base_url = api_hub_base_url.trim_end_matches('/').to_string();
        let s3_bucket = vars
            .get("S3_BUCKET")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_BUCKET.to_string());
        let camera_id = vars
            .get("CAMERA_ID")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_CAMERA_ID.to_string());

        let record_seconds = match vars.get("RECORD_SECONDS") {
            Some(v) => {
                let parsed = v.parse::<u64>().map_err(|_| AppError::InvalidEnv {
                    name: "RECORD_SECONDS",
                    reason: "must be an integer".to_string(),
                })?;
                if parsed == 0 {
                    return Err(AppError::InvalidEnv {
                        name: "RECORD_SECONDS",
                        reason: "must be > 0".to_string(),
                    });
                }
                parsed
            }
            None => Self::DEFAULT_RECORD_SECONDS,
        };

        let ffmpeg_video_codec = vars
            .get("FFMPEG_VIDEO_CODEC")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_FFMPEG_VIDEO_CODEC.to_string());
        if ffmpeg_video_codec.trim().is_empty() {
            return Err(AppError::InvalidEnv {
                name: "FFMPEG_VIDEO_CODEC",
                reason: "must not be empty".to_string(),
            });
        }

        let ffmpeg_audio_codec = vars
            .get("FFMPEG_AUDIO_CODEC")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_FFMPEG_AUDIO_CODEC.to_string());
        if ffmpeg_audio_codec.trim().is_empty() {
            return Err(AppError::InvalidEnv {
                name: "FFMPEG_AUDIO_CODEC",
                reason: "must not be empty".to_string(),
            });
        }

        let ffmpeg_audio_bitrate = vars
            .get("FFMPEG_AUDIO_BITRATE")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_FFMPEG_AUDIO_BITRATE.to_string());
        if ffmpeg_audio_bitrate.trim().is_empty() {
            return Err(AppError::InvalidEnv {
                name: "FFMPEG_AUDIO_BITRATE",
                reason: "must not be empty".to_string(),
            });
        }

        let ffmpeg_loglevel = vars
            .get("FFMPEG_LOGLEVEL")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_FFMPEG_LOGLEVEL.to_string());
        if ffmpeg_loglevel.trim().is_empty() {
            return Err(AppError::InvalidEnv {
                name: "FFMPEG_LOGLEVEL",
                reason: "must not be empty".to_string(),
            });
        }

        let ffmpeg_preset = vars
            .get("FFMPEG_PRESET")
            .cloned()
            .unwrap_or_else(|| Self::DEFAULT_FFMPEG_PRESET.to_string());
        if ffmpeg_preset.trim().is_empty() {
            return Err(AppError::InvalidEnv {
                name: "FFMPEG_PRESET",
                reason: "must not be empty".to_string(),
            });
        }

        let ffmpeg_crf = match vars.get("FFMPEG_CRF") {
            Some(v) => {
                let parsed = v.parse::<u8>().map_err(|_| AppError::InvalidEnv {
                    name: "FFMPEG_CRF",
                    reason: "must be an integer".to_string(),
                })?;
                if parsed > 51 {
                    return Err(AppError::InvalidEnv {
                        name: "FFMPEG_CRF",
                        reason: "must be <= 51".to_string(),
                    });
                }
                parsed
            }
            None => Self::DEFAULT_FFMPEG_CRF,
        };

        Ok(Self {
            rtsp_url,
            api_hub_base_url,
            s3_bucket,
            camera_id,
            record_seconds,
            ffmpeg_video_codec,
            ffmpeg_audio_codec,
            ffmpeg_audio_bitrate,
            ffmpeg_loglevel,
            ffmpeg_preset,
            ffmpeg_crf,
        })
    }
}

fn required(vars: &HashMap<String, String>, key: &'static str) -> Result<String, AppError> {
    vars.get(key).cloned().ok_or(AppError::MissingEnv(key))
}
