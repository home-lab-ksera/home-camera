use std::path::Path;
use std::process::Command;
use std::time::Duration;

use reqwest::Url;

use crate::error::AppError;

#[derive(Debug, Clone, Copy)]
pub struct RecorderStats {
    pub recorded_seconds: u64,
}

pub fn record_to_mp4(
    rtsp_url: &str,
    output_path: &Path,
    duration: Duration,
    ffmpeg_loglevel: &str,
    ffmpeg_video_codec: &str,
    ffmpeg_audio_codec: &str,
    ffmpeg_audio_bitrate: &str,
    ffmpeg_preset: &str,
    ffmpeg_crf: u8,
) -> Result<RecorderStats, AppError> {
    validate_rtsp_url(rtsp_url)?;

    let duration_secs = duration.as_secs().max(1);
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg(ffmpeg_loglevel)
        .arg("-rtsp_transport")
        .arg("tcp")
        .arg("-fflags")
        .arg("+genpts+discardcorrupt")
        .arg("-use_wallclock_as_timestamps")
        .arg("1")
        .arg("-i")
        .arg(rtsp_url)
        .arg("-t")
        .arg(duration_secs.to_string())
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a?")
        .arg("-c:v")
        .arg(ffmpeg_video_codec)
        .arg("-c:a")
        .arg(ffmpeg_audio_codec);

    if ffmpeg_video_codec.eq_ignore_ascii_case("libx264") {
        command
            .arg("-preset")
            .arg(ffmpeg_preset)
            .arg("-crf")
            .arg(ffmpeg_crf.to_string())
            .arg("-pix_fmt")
            .arg("yuv420p");
    }

    if !ffmpeg_audio_codec.eq_ignore_ascii_case("copy") {
        command.arg("-b:a").arg(ffmpeg_audio_bitrate);
    }

    let output = command
        .arg("-movflags")
        .arg("+faststart")
        .arg("-y")
        .arg(output_path)
        .output()
        .map_err(|e| AppError::Recording(format!("failed to start ffmpeg: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).replace(rtsp_url, "[camera stream]");
        let lines: Vec<_> = stderr.lines().collect();
        let reason = if lines.is_empty() {
            "unknown ffmpeg error".to_string()
        } else {
            lines
                .iter()
                .rev()
                .take(10)
                .rev()
                .copied()
                .collect::<Vec<_>>()
                .join(" | ")
        };
        return Err(AppError::Recording(format!(
            "ffmpeg failed with status {}: {reason}",
            output.status
        )));
    }

    Ok(RecorderStats {
        recorded_seconds: duration_secs,
    })
}

fn validate_rtsp_url(rtsp_url: &str) -> Result<(), AppError> {
    let uri = Url::parse(rtsp_url).map_err(|e| AppError::Uri(e.to_string()))?;
    let scheme = uri.scheme().to_ascii_lowercase();
    if scheme != "rtsp" {
        return Err(AppError::Recording(
            "RTSP_URL scheme must be rtsp".to_string(),
        ));
    }
    if uri.host_str().is_none() {
        return Err(AppError::Recording(
            "RTSP_URL must include host".to_string(),
        ));
    }
    Ok(())
}
