use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use home_camera::config::AppConfig;
use home_camera::error::AppError;
use home_camera::key::build_object_key;
use home_camera::recorder::record_to_mp4;
use home_camera::slack_client::post_message;
use home_camera::storage::upload_file;
use reqwest::Client;
use time::{OffsetDateTime, UtcOffset};

const JST_OFFSET_HOURS: i8 = 9;

#[tokio::main]
async fn main() -> Result<()> {
    if home_camera::persistent::dispatch().await? {
        return Ok(());
    }
    let http = Client::builder().build()?;
    let _ = post_message(&http, "log", "camera start").await;
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    let _ = post_message(&http, "log", "camera finish").await;
    Ok(())
}

async fn run() -> Result<(), AppError> {
    let config = AppConfig::from_env()?;
    let jst = UtcOffset::from_hms(JST_OFFSET_HOURS, 0, 0)?;
    let now = OffsetDateTime::now_utc().to_offset(jst);
    let object_key = build_object_key(&config.camera_id, now);

    let temp_file = temp_mp4_path(&config.camera_id, now.unix_timestamp());
    println!(
        "recording started: camera={}",
        home_camera::key::sanitize_camera_id(&config.camera_id)
    );
    println!("temp file: {}", temp_file.display());

    let stats = record_to_mp4(
        &config.rtsp_url,
        &temp_file,
        Duration::from_secs(config.record_seconds),
        &config.ffmpeg_loglevel,
        &config.ffmpeg_video_codec,
        &config.ffmpeg_audio_codec,
        &config.ffmpeg_audio_bitrate,
        &config.ffmpeg_preset,
        config.ffmpeg_crf,
    )?;

    println!(
        "recording completed: recorded_seconds={}",
        stats.recorded_seconds
    );
    println!("uploading to s3://{}/{}", config.s3_bucket, object_key);

    upload_file(&config, &object_key, &temp_file).await?;
    println!("upload completed");

    let _ = std::fs::remove_file(&temp_file);
    Ok(())
}

fn temp_mp4_path(camera_id: &str, unix_ts: i64) -> PathBuf {
    let safe_id: String = camera_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from(format!("/tmp/{safe_id}-{unix_ts}.mp4"))
}
