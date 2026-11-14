//! 完成した segment を1件ずつ S3 へ転送する。

use super::*;

pub(super) async fn uploader() -> Result<()> {
    let root = root();
    let _lock = lock(&root, "uploader.lock")?;
    // uploaderはカメラに接続せず、RTSP資格情報も必要としない。
    let mut vars: HashMap<String, String> = env::vars().collect();
    vars.entry("RTSP_URL".into())
        .or_insert_with(|| "rtsp://unused".into());
    let config = AppConfig::from_map(&vars)?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(3600))
        .build()?;
    loop {
        if let Err(error) = upload_one(&root, &config, &http).await {
            eprintln!("camera upload retry: {error:#}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub(super) async fn transfer(
    segment: &Segment,
    file: &Path,
    config: &AppConfig,
    http: &reqwest::Client,
) -> Result<()> {
    let local = present(file.metadata())?;
    if let Some(metadata) = &local
        && (metadata.len() != segment.size || checksum(file)? != segment.checksum)
    {
        bail!("spool file changed; preserving for inspection");
    }
    let head = || {
        http.post(format!("{}/s3/head_object", config.api_hub_base_url))
            .json(&json!({"bucket":config.s3_bucket,"key":segment.object_key}))
            .send()
    };
    let mut response = head().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        if local.is_none() {
            bail!("recording missing from both spool and S3");
        }
        crate::storage::upload_file(config, &segment.object_key, file).await?;
        response = head().await?;
    }
    let value: Value = response.error_for_status()?.json().await?;
    // remote checksum保証ではなく固定キーとサイズの照合。
    if value["content_length"].as_u64() != Some(segment.size) {
        bail!("stored recording size mismatch");
    }
    Ok(())
}

pub(super) async fn upload_one(
    root: &Path,
    config: &AppConfig,
    http: &reqwest::Client,
) -> Result<bool> {
    let mut attempted = false;
    for path in manifests(root)? {
        let mut segment = match read_segment(&path) {
            Ok(segment) => segment,
            Err(error) => {
                eprintln!("invalid camera manifest {}: {error}", path.display());
                continue;
            }
        };
        if segment.next_attempt_at > OffsetDateTime::now_utc().unix_timestamp() {
            continue;
        }
        attempted = true;
        let file = path
            .parent()
            .context("session directory missing")?
            .join(&segment.file_name);
        match transfer(&segment, &file, config, http).await {
            Ok(()) => {
                // 動画を先に削除。途中停止で残ったmanifestは次回S3を照合して解放する。
                present(fs::remove_file(&file))?;
                fs::remove_file(&path)?;
                File::open(path.parent().unwrap())?.sync_all()?;
            }
            Err(error) => {
                eprintln!("camera upload retry for {}: {error:#}", segment.id);
                segment.attempts = segment.attempts.saturating_add(1);
                segment.next_attempt_at = OffsetDateTime::now_utc().unix_timestamp()
                    + (10_i64 * 2_i64.pow(segment.attempts.min(7))).min(900);
                atomic_json(&path, &segment)?;
            }
        }
    }
    // 稼働中・異常を含むsessionを保持し、CSVだけになった完了sessionを整理する。
    for session in sessions(root)? {
        if session.join(".active").exists()
            || !session
                .metadata()?
                .modified()?
                .elapsed()
                .is_ok_and(|age| age > Duration::from_secs(60))
        {
            continue;
        }
        let files: Vec<_> = fs::read_dir(&session)?.collect::<std::io::Result<Vec<_>>>()?;
        if files.iter().all(|file| file.file_name() == "segments.csv") {
            for file in files {
                fs::remove_file(file.path())?;
            }
            fs::remove_dir(session)?;
        }
    }
    Ok(attempted)
}
