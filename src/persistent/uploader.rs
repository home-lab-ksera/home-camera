//! 完成した segment を1件ずつ S3 へ転送し、続けて動きのあった区間を解析して結果を送る。

use super::*;
use crate::motion::{Params, analyze_file_within};

/// 録画1本（1時間）の解析にかける時間の上限。超えたら解析を諦めて削除へ進む。
const MOTION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 解析結果の送信をこの回数まで試す。それ以上は spool を空けるため諦める。
pub(super) const MOTION_PUT_ATTEMPTS: u32 = 3;

/// 解析の設定。`MOTION_ANALYSIS=off` か設定の誤りなら解析しない（録画の送信は続ける）。
fn motion_params() -> Option<Params> {
    if env::var("MOTION_ANALYSIS").is_ok_and(|v| v == "off") {
        return None;
    }
    match Params::from_env() {
        Ok(params) => Some(params),
        Err(error) => {
            eprintln!("camera motion analysis disabled: {error:#}");
            None
        }
    }
}

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
    let motion = motion_params();
    loop {
        if let Err(error) = upload_one(&root, &config, &http, motion.as_ref()).await {
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

/// `camera/.../x.mp4` に対する解析結果のキー `camera/.../x.motion.json`。
pub(super) fn motion_key(object_key: &str) -> String {
    format!(
        "{}.motion.json",
        object_key.strip_suffix(".mp4").unwrap_or(object_key)
    )
}

/// 解析結果を作る。FFmpeg の失敗は再試行しても直らないことが多いので、
/// 失敗も結果として残し、朝の通知で「解析できなかった」と分かるようにする。
async fn motion_report(file: &Path, params: &Params) -> Vec<u8> {
    let path = file.to_path_buf();
    let params = params.clone();
    let result =
        tokio::task::spawn_blocking(move || analyze_file_within(&path, &params, MOTION_TIMEOUT))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
    let value = match result {
        Ok(report) => json!(report),
        Err(error) => {
            eprintln!(
                "camera motion analysis failed for {}: {error:#}",
                file.display()
            );
            let message: String = format!("{error:#}").chars().take(500).collect();
            json!({"version": 1, "error": message})
        }
    };
    value.to_string().into_bytes()
}

/// 送信済みの録画を解析し、結果を S3 に置く。結果はまず spool に保存し、
/// 送信に失敗しても解析をやり直さない。spool に録画が無ければ（削除の途中で止まった）何もしない。
async fn publish_motion(
    segment: &Segment,
    file: &Path,
    config: &AppConfig,
    params: &Params,
) -> Result<()> {
    let local = file.with_extension("motion");
    let report = match present(fs::read(&local))? {
        Some(report) => report,
        None => {
            if !file.exists() {
                return Ok(());
            }
            let report = motion_report(file, params).await;
            let temp = file.with_extension("motion.tmp");
            fs::write(&temp, &report)?;
            fs::rename(&temp, &local)?;
            report
        }
    };
    crate::storage::upload_bytes(
        config,
        &motion_key(&segment.object_key),
        "application/json",
        report,
    )
    .await?;
    Ok(())
}

pub(super) async fn upload_one(
    root: &Path,
    config: &AppConfig,
    http: &reqwest::Client,
    motion: Option<&Params>,
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
        let mut result = transfer(&segment, &file, config, http).await;
        if result.is_ok()
            && let Some(params) = motion
            && let Err(error) = publish_motion(&segment, &file, config, params).await
        {
            segment.motion_attempts = segment.motion_attempts.saturating_add(1);
            if segment.motion_attempts < MOTION_PUT_ATTEMPTS {
                result = Err(error.context("motion report upload"));
            } else {
                // 録画は送信済み。解析結果のために spool を埋め続けない。
                eprintln!("camera motion report dropped for {}: {error:#}", segment.id);
            }
        }
        match result {
            Ok(()) => {
                // 動画を先に削除。途中停止で残ったmanifestは次回S3を照合して解放する。
                for extension in ["motion.tmp", "motion"] {
                    present(fs::remove_file(file.with_extension(extension)))?;
                }
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
