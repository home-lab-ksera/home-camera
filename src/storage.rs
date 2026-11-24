use std::path::Path;

use reqwest::{Body, Client};
use tokio::fs::File;
use tokio_util::io::ReaderStream;

use crate::config::AppConfig;
use crate::error::AppError;

pub async fn upload_file(config: &AppConfig, key: &str, path: &Path) -> Result<(), AppError> {
    let file = File::open(path)
        .await
        .map_err(|e| AppError::S3Upload(e.to_string()))?;
    let size = file
        .metadata()
        .await
        .map_err(|e| AppError::S3Upload(e.to_string()))?
        .len();
    if size == 0 {
        return Err(AppError::S3Upload("recording file is empty".to_string()));
    }
    // 録画全体をメモリに載せず、multipart upload の管理は api-hub に委譲する。
    put(
        config,
        key,
        "video/mp4",
        size,
        Body::wrap_stream(ReaderStream::new(file)),
    )
    .await
}

/// 小さなファイル（動きの解析結果など）をそのまま送る。
pub async fn upload_bytes(
    config: &AppConfig,
    key: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> Result<(), AppError> {
    let size = bytes.len() as u64;
    put(config, key, content_type, size, Body::from(bytes)).await
}

async fn put(
    config: &AppConfig,
    key: &str,
    content_type: &str,
    size: u64,
    body: Body,
) -> Result<(), AppError> {
    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| AppError::S3Upload(e.to_string()))?
        .post(format!("{}/s3/put_object_stream", config.api_hub_base_url))
        .query(&[("bucket", config.s3_bucket.as_str()), ("key", key)])
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::CONTENT_LENGTH, size)
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::S3Upload(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(AppError::S3Upload(format!(
            "api-hub upload failed: HTTP {status}"
        )));
    }
    // レスポンスの受信完了まで確認してから呼び出し側が録画ファイルを削除する。
    response
        .bytes()
        .await
        .map_err(|e| AppError::S3Upload(e.to_string()))?;
    Ok(())
}
