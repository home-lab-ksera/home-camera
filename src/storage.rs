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
    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AppError::S3Upload(e.to_string()))?
        .post(format!("{}/s3/put_object_stream", config.api_hub_base_url))
        .query(&[("bucket", config.s3_bucket.as_str()), ("key", key)])
        .header(reqwest::header::CONTENT_TYPE, "video/mp4")
        .header(reqwest::header::CONTENT_LENGTH, size)
        .body(Body::wrap_stream(ReaderStream::new(file)))
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
