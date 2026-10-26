//! PVC上の完成動画とmanifestを正本にし、単一uploaderがS3へ転送する。
use crate::{config::AppConfig, key::sanitize_camera_id};
use anyhow::{Context, Result, bail};
use axum::{Router, extract::State, http::StatusCode, routing::get};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tokio::process::{Child, Command};

#[derive(Serialize, Deserialize, Debug)]
struct Segment {
    id: String,
    camera_id: String,
    actual_start_at: String,
    actual_end_at: String,
    file_name: String,
    object_key: String,
    size: u64,
    checksum: String,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    next_attempt_at: i64,
}

fn read_segment(path: &Path) -> Result<Segment> {
    let segment: Segment = serde_json::from_reader(File::open(path)?)?;
    let start = OffsetDateTime::parse(&segment.actual_start_at, &Rfc3339)?;
    let end = OffsetDateTime::parse(&segment.actual_end_at, &Rfc3339)?;
    if segment.id.is_empty()
        || segment.camera_id.is_empty()
        || segment.object_key.is_empty()
        || segment.size == 0
        || end <= start
        || segment.checksum.len() != 64
        || !segment.checksum.bytes().all(|b| b.is_ascii_hexdigit())
        || Path::new(&segment.file_name)
            .file_name()
            .and_then(|s| s.to_str())
            != Some(segment.file_name.as_str())
        || path
            .with_extension("mp4")
            .file_name()
            .and_then(|s| s.to_str())
            != Some(segment.file_name.as_str())
        || segment.next_attempt_at < 0
    {
        bail!("invalid segment manifest; preserving for inspection");
    }
    Ok(segment)
}

pub async fn dispatch() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 1 {
        bail!("Usage: home-camera --record|--upload");
    }
    match args[0].as_str() {
        "--record" => recorder().await?,
        "--upload" => uploader().await?,
        _ => bail!("Usage: home-camera --record|--upload"),
    }
    Ok(())
}

fn root() -> PathBuf {
    env::var_os("SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/spool"))
}
fn lock(root: &Path, name: &str) -> Result<File> {
    fs::create_dir_all(root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(name))?;
    file.try_lock_exclusive()
        .context("another process already owns this spool")?;
    Ok(file)
}
fn checksum(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut chunk = [0; 65536];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hash.update(&chunk[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    File::open(path.parent().context("manifest directory missing")?)?.sync_all()?;
    Ok(())
}
fn sessions(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect())
}
fn manifests(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for session in sessions(root)? {
        for file in fs::read_dir(&session)? {
            let file = file?.path();
            if file.extension().is_some_and(|value| value == "json") {
                files.push(file);
            }
        }
    }
    Ok(files)
}

async fn finish(path: &Path, camera: &str) -> Result<()> {
    let stem = path
        .file_stem()
        .context("segment filename missing")?
        .to_str()
        .context("invalid segment filename")?;
    let epoch = stem
        .strip_prefix("segment-")
        .context("invalid segment filename")?
        .parse::<i64>()?;
    let start = OffsetDateTime::from_unix_timestamp(epoch)?;
    let probe = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(env::var("FFPROBE_BIN").unwrap_or_else(|_| "ffprobe".into()))
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "json",
            ])
            .arg(path)
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !probe.status.success() {
        bail!("segment is incomplete");
    }
    let value: Value = serde_json::from_slice(&probe.stdout)?;
    let duration = value["format"]["duration"]
        .as_str()
        .context("segment duration missing")?
        .parse::<f64>()?;
    if !duration.is_finite() || !(0.01..=86400.0).contains(&duration) {
        bail!("invalid segment duration");
    }
    let end = start + time::Duration::milliseconds((duration * 1000.0) as i64);
    let parent = path.parent().context("session directory missing")?;
    let session = parent
        .file_name()
        .context("session ID missing")?
        .to_str()
        .context("invalid session ID")?;
    let mp4 = path.with_extension("mp4");
    if !mp4.exists() {
        fs::rename(path, &mp4)?;
    }
    let metadata = mp4.metadata()?;
    if metadata.len() == 0 {
        bail!("empty segment");
    }
    let camera = sanitize_camera_id(camera);
    let segment = Segment {
        id: format!("{session}-{epoch}"),
        camera_id: camera.clone(),
        actual_start_at: start.format(&Rfc3339)?,
        actual_end_at: end.format(&Rfc3339)?,
        file_name: mp4.file_name().unwrap().to_str().unwrap().into(),
        object_key: format!(
            "{camera}/{:04}/{:02}/{:02}/{epoch}-{session}.mp4",
            start.year(),
            start.month() as u8,
            start.day()
        ),
        size: metadata.len(),
        checksum: checksum(&mp4)?,
        attempts: 0,
        next_attempt_at: 0,
    };
    atomic_json(&mp4.with_extension("json"), &segment)?;
    Ok(())
}

async fn recover(root: &Path, camera: &str) -> Result<u64> {
    let mut completed = 0;
    for session in sessions(root)? {
        for entry in fs::read_dir(&session)? {
            let path = entry?.path();
            if !path.extension().is_some_and(|v| v == "part" || v == "mp4")
                || path.with_extension("json").exists()
            {
                continue;
            }
            match finish(&path, camera).await {
                Ok(()) => completed += 1,
                Err(error) => {
                    eprintln!("camera segment could not be recovered: {error}");
                    if path.extension().is_some_and(|v| v == "part") {
                        fs::rename(&path, path.with_extension("corrupt"))?;
                    }
                }
            }
        }
        if session.join(".active").exists() {
            fs::remove_file(session.join(".active"))?;
        }
    }
    Ok(completed)
}

#[derive(Default)]
struct RecordingProgress {
    // 録画開始の猶予、または最後に正常確定したsegmentからの経過時間。
    // FFmpegの再起動では更新しない。
    since: Option<Instant>,
}

impl RecordingProgress {
    fn observe(&mut self, active_time: bool, blocked: bool, now: Instant) {
        if active_time && !blocked {
            self.since.get_or_insert(now);
        } else {
            self.since = None;
        }
    }

    fn completed(&mut self, now: Instant) {
        if self.since.is_some() {
            self.since = Some(now);
        }
    }

    fn stalled(&self, now: Instant, deadline: Duration) -> bool {
        self.since
            .is_some_and(|since| now.duration_since(since) > deadline)
    }
}

#[derive(Default)]
struct Metrics {
    bytes: u64,
    oldest_age: f64,
    blocked: bool,
    active: bool,
    partial: u64,
    invalid_manifests: u64,
    stalled: bool,
    restarts: u64,
    heartbeat: Option<Instant>,
    stats_success: bool,
}
// uploaderは独立して送信済みファイル・sessionを削除する。走査後の消失は正常。
fn present<T>(result: std::io::Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn stats(root: &Path) -> Result<(u64, f64, u64, u64)> {
    let mut bytes = 0;
    let mut oldest = OffsetDateTime::now_utc().unix_timestamp();
    let mut partial = 0;
    let mut invalid = 0;
    for session in sessions(root)? {
        let Some(entries) = present(fs::read_dir(&session))? else {
            continue;
        };
        for entry in entries {
            let Some(entry) = present(entry)? else {
                continue;
            };
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|v| v == "mp4" || v == "part" || v == "corrupt")
            {
                let Some(metadata) = present(entry.metadata())? else {
                    continue;
                };
                bytes += metadata.len();
            }
            if path.extension().is_some_and(|v| v == "corrupt") {
                partial += 1;
            }
            if path.extension().is_some_and(|v| v == "json") {
                match read_segment(&path) {
                    Ok(segment) => {
                        oldest = oldest.min(
                            OffsetDateTime::parse(&segment.actual_start_at, &Rfc3339)?
                                .unix_timestamp(),
                        )
                    }
                    Err(_) if !path.exists() => {} // uploaderによる削除と競合した場合
                    Err(_) => invalid += 1,
                }
            }
        }
    }
    Ok((
        bytes,
        (OffsetDateTime::now_utc().unix_timestamp() - oldest).max(0) as f64,
        partial,
        invalid,
    ))
}
fn allowed(now: OffsetDateTime) -> bool {
    let hour = now.to_offset(UtcOffset::from_hms(9, 0, 0).unwrap()).hour();
    hour >= 19 || hour < 9
}
async fn stop(child: &mut Child) {
    if let Some(id) = child.id() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(id as i32),
            nix::sys::signal::Signal::SIGINT,
        );
    }
    if tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

fn rtsp_command(url: &str, timeout_us: u64) -> Command {
    let mut command = Command::new(env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".into()));
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-rtsp_transport",
        "tcp",
        "-timeout",
        &timeout_us.to_string(),
        "-i",
        url,
    ]);
    command
}

async fn recorder() -> Result<()> {
    let config = AppConfig::from_env()?;
    let root = root();
    let _lock = lock(&root, "recorder.lock")?;
    recover(&root, &config.camera_id).await?;
    let seconds: u64 = env::var("SEGMENT_SECONDS")
        .unwrap_or_else(|_| "300".into())
        .parse()?;
    if !(10..=900).contains(&seconds) {
        bail!("SEGMENT_SECONDS must be 10..900");
    }
    let max: u64 = env::var("SPOOL_MAX_BYTES")
        .unwrap_or_else(|_| "85899345920".into())
        .parse()?;
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let router=Router::new().route("/health",get(|State(value):State<Arc<Mutex<Metrics>>>|async move{if value.lock().unwrap().heartbeat.is_some_and(|time|time.elapsed()<Duration::from_secs(60)){StatusCode::OK}else{StatusCode::SERVICE_UNAVAILABLE}}))
        .route("/metrics",get(|State(value):State<Arc<Mutex<Metrics>>>|async move{let v=value.lock().unwrap();format!("home_camera_spool_bytes {}\nhome_camera_spool_oldest_age_seconds {}\nhome_camera_spool_blocked {}\nhome_camera_recording_active {}\nhome_camera_partial_segments {}\nhome_camera_recorder_restarts_total {}\nhome_camera_spool_stats_success {}\nhome_camera_invalid_manifests {}\nhome_camera_recording_stalled {}\n",v.bytes,v.oldest_age,u8::from(v.blocked),u8::from(v.active),v.partial,v.restarts,u8::from(v.stats_success),v.invalid_manifests,u8::from(v.stalled))})).with_state(metrics.clone());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:9090").await?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut child: Option<(Child, PathBuf)> = None;
    let mut next_start = Instant::now();
    let mut sampled = (0, 0.0, 0, 0);
    let mut process_progress = Instant::now();
    let mut recording_progress = RecordingProgress::default();
    let progress_deadline = Duration::from_secs(seconds * 2 + 60);
    let mut next_sample = Instant::now();
    loop {
        if Instant::now() >= next_sample {
            match stats(&root) {
                Ok(value) => {
                    sampled = value;
                    metrics.lock().unwrap().stats_success = true;
                }
                Err(error) => {
                    // 集計の失敗でFFmpegを停止しない。直近の値を保持し監視へ失敗を伝える。
                    metrics.lock().unwrap().stats_success = false;
                    eprintln!("camera spool statistics failed: {error:#}");
                }
            }
            next_sample = Instant::now() + Duration::from_secs(15);
        }
        let (bytes, age, partial, invalid) = sampled;
        let blocked = bytes >= max || fs2::available_space(&root)? < 2 * 1024 * 1024 * 1024;
        let active_time = allowed(OffsetDateTime::now_utc());
        recording_progress.observe(active_time, blocked, Instant::now());
        {
            let mut v = metrics.lock().unwrap();
            v.bytes = bytes;
            v.oldest_age = age;
            v.partial = partial;
            v.invalid_manifests = invalid;
            v.blocked = blocked;
            v.active = child.is_some();
            v.heartbeat = Some(Instant::now());
        }
        if child.is_none() && active_time && !blocked && Instant::now() >= next_start {
            let session = root.join(uuid::Uuid::new_v4().to_string());
            fs::create_dir(&session)?;
            File::create(session.join(".active"))?;
            let mut command = rtsp_command(&config.rtsp_url, 30_000_000);
            command.args([
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-c:v",
                &config.ffmpeg_video_codec,
                "-c:a",
                &config.ffmpeg_audio_codec,
            ]);
            if config.ffmpeg_video_codec == "libx264" {
                command.args([
                    "-preset",
                    &config.ffmpeg_preset,
                    "-crf",
                    &config.ffmpeg_crf.to_string(),
                    "-pix_fmt",
                    "yuv420p",
                ]);
            }
            if config.ffmpeg_audio_codec != "copy" {
                command.args(["-b:a", &config.ffmpeg_audio_bitrate]);
            }
            command
                .args([
                    "-f",
                    "segment",
                    "-segment_time",
                    &seconds.to_string(),
                    "-reset_timestamps",
                    "1",
                    "-strftime",
                    "1",
                    "-segment_format",
                    "mp4",
                    "-segment_format_options",
                    "movflags=+faststart",
                    "-segment_list_type",
                    "csv",
                    "-segment_list_size",
                    "0",
                    "-segment_list",
                ])
                .arg(session.join("segments.csv"))
                .arg(session.join("segment-%s.part"))
                .env("TZ", "UTC")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            child = Some((command.spawn().context("could not start ffmpeg")?, session));
            process_progress = Instant::now();
        }
        if let Some((process, session)) = &mut child {
            // muxerが閉じたファイルだけをCSVから確定する。現在書き込み中のpartは送らない。
            if let Ok(csv) = fs::read_to_string(session.join("segments.csv")) {
                for line in csv.lines() {
                    if let Some(name) = line.split(',').next() {
                        let path = session.join(name.trim_matches('"'));
                        if path.exists() && path.extension().is_some_and(|v| v == "part") {
                            match finish(&path, &config.camera_id).await {
                                Ok(()) => {
                                    process_progress = Instant::now();
                                    recording_progress.completed(process_progress);
                                }
                                Err(error) => {
                                    eprintln!("closed segment could not be finalized: {error}");
                                    fs::rename(&path, path.with_extension("corrupt"))?;
                                }
                            }
                        }
                    }
                }
            }
            let exited = process.try_wait()?.is_some();
            // 接続猶予60秒とkeyframe境界の余裕を含め、完成segmentの停止を検知する。
            let stalled = process_progress.elapsed() > progress_deadline;
            if exited || stalled || !active_time || blocked {
                stop(process).await;
                child = None;
                if recover(&root, &config.camera_id).await? > 0 {
                    recording_progress.completed(Instant::now());
                }
                next_start = Instant::now() + Duration::from_secs(10);
                metrics.lock().unwrap().restarts += u64::from(exited || stalled);
            }
        }
        metrics.lock().unwrap().stalled =
            recording_progress.stalled(Instant::now(), progress_deadline);
        tokio::select! {_=terminate.recv()=>break,_=interrupt.recv()=>break,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
    }
    if let Some((mut process, _)) = child {
        stop(&mut process).await;
        recover(&root, &config.camera_id).await?;
    }
    server.abort();
    Ok(())
}

async fn uploader() -> Result<()> {
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

async fn transfer(
    segment: &Segment,
    file: &Path,
    config: &AppConfig,
    http: &reqwest::Client,
) -> Result<()> {
    let local = present(file.metadata())?;
    if let Some(metadata) = &local {
        if metadata.len() != segment.size || checksum(file)? != segment.checksum {
            bail!("spool file changed; preserving for inspection");
        }
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

async fn upload_one(root: &Path, config: &AppConfig, http: &reqwest::Client) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json,
        body::Bytes,
        extract::Query,
        response::{IntoResponse, Response},
        routing::post,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn recording_deadline_survives_reconnects_until_a_segment_completes() {
        let start = Instant::now();
        let deadline = Duration::from_secs(80);
        let mut progress = RecordingProgress::default();
        // 10秒おきの再接続でも録画成功の時計は進み続ける。
        for seconds in (0..=100).step_by(10) {
            let now = start + Duration::from_secs(seconds);
            progress.observe(true, false, now);
            assert_eq!(progress.stalled(now, deadline), seconds > 80);
        }
        let completed = start + Duration::from_secs(101);
        progress.completed(completed);
        assert!(!progress.stalled(completed, deadline));
        assert!(!progress.stalled(completed + deadline, deadline));
        assert!(progress.stalled(completed + deadline + Duration::from_secs(1), deadline));
    }

    #[test]
    fn recording_window_and_capacity_stop_give_a_new_startup_grace() {
        let start = Instant::now();
        let deadline = Duration::from_secs(80);
        for (active_time, blocked) in [(false, false), (true, true)] {
            let mut progress = RecordingProgress::default();
            progress.observe(true, false, start);
            assert!(progress.stalled(start + Duration::from_secs(100), deadline));
            progress.observe(active_time, blocked, start + Duration::from_secs(101));
            assert!(!progress.stalled(start + Duration::from_secs(1000), deadline));
            // 停止時の回収segmentで、時間帯外の監視を再開しない。
            progress.completed(start + Duration::from_secs(1000));
            assert!(progress.since.is_none());
            let resumed = start + Duration::from_secs(2000);
            progress.observe(true, false, resumed);
            assert!(!progress.stalled(resumed + deadline, deadline));
            assert!(progress.stalled(resumed + deadline + Duration::from_secs(1), deadline));
        }
    }

    #[test]
    fn recording_window_crosses_midnight_in_jst() {
        for (utc, expected) in [
            ("2026-10-24T09:59:00Z", false),
            ("2026-10-24T10:00:00Z", true),
            ("2026-10-24T23:59:00Z", true),
            ("2026-10-25T00:00:00Z", false),
        ] {
            assert_eq!(
                allowed(OffsetDateTime::parse(utc, &Rfc3339).unwrap()),
                expected
            );
        }
    }
    #[test]
    fn spool_has_one_writer() {
        let root = tempfile::tempdir().unwrap();
        let first = lock(root.path(), "recorder.lock").unwrap();
        assert!(lock(root.path(), "recorder.lock").is_err());
        drop(first);
        assert!(lock(root.path(), "recorder.lock").is_ok());
    }

    #[test]
    fn spool_statistics_tolerate_uploader_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session");
        fs::create_dir(&session).unwrap();
        for i in 0..1000 {
            fs::write(session.join(format!("{i}.mp4")), [0_u8]).unwrap();
        }
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = running.clone();
        let writer = std::thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                for i in 0..1000 {
                    let path = session.join(format!("{i}.mp4"));
                    fs::remove_file(&path).unwrap();
                    fs::write(path, [0_u8]).unwrap();
                }
            }
        });
        // 修正前はmetadata()のENOENTがここから録画ループへ伝播する。
        let result = (0..100).try_for_each(|_| stats(root.path()).map(|_| ()));
        running.store(false, Ordering::Relaxed);
        writer.join().unwrap();
        result.unwrap();
        // 消失以外のI/O障害は隠さず、recorderの集計失敗メトリクスへ伝える。
        assert!(present::<File>(Err(std::io::ErrorKind::PermissionDenied.into())).is_err());
        assert!(
            present(File::open(root.path().join("removed.json")))
                .unwrap()
                .is_none()
        );
        assert!(
            present(fs::read_dir(root.path().join("removed-session")))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires FFMPEG_BIN and FFPROBE_BIN"]
    async fn segment_boundaries_keep_recording_with_pending_uploads() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("continuous");
        fs::create_dir(&session).unwrap();
        let input = root.path().join("input.mp4");
        fs::write(&input, include_bytes!("../tests/fixtures/blue.mp4")).unwrap();
        let mut child = Command::new(env::var("FFMPEG_BIN").expect("FFMPEG_BIN required"))
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-re",
                "-stream_loop",
                "-1",
                "-i",
            ])
            .arg(input)
            .args([
                "-map",
                "0:v:0",
                "-c",
                "copy",
                "-f",
                "segment",
                "-segment_time",
                "1",
                "-reset_timestamps",
                "1",
                "-strftime",
                "1",
                "-segment_format",
                "mp4",
                "-segment_format_options",
                "movflags=+faststart",
                "-segment_list_type",
                "csv",
                "-segment_list_size",
                "0",
                "-segment_list",
            ])
            .arg(session.join("segments.csv"))
            .arg(session.join("segment-%s.part"))
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while manifests(root.path()).unwrap().len() < 2 && Instant::now() < deadline {
            assert!(child.try_wait().unwrap().is_none());
            if let Ok(csv) = fs::read_to_string(session.join("segments.csv")) {
                for line in csv.lines() {
                    let path = session.join(line.split(',').next().unwrap().trim_matches('"'));
                    if path.exists() {
                        finish(&path, "camera").await.unwrap();
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(manifests(root.path()).unwrap().len() >= 2);
        // 先のsegmentが未送信のままでも、同じffmpegが次のsegmentを記録している。
        assert!(child.try_wait().unwrap().is_none());
        stop(&mut child).await;
        recover(root.path(), "camera").await.unwrap();
        assert!(stats(root.path()).unwrap().0 > 0);
    }

    #[derive(Clone, Default)]
    struct Mock {
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        uploads: Arc<AtomicUsize>,
        unavailable: Arc<std::sync::atomic::AtomicBool>,
    }
    async fn head(State(mock): State<Mock>, Json(value): Json<Value>) -> Response {
        if mock.unavailable.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        match mock
            .objects
            .lock()
            .unwrap()
            .get(value["key"].as_str().unwrap())
        {
            Some(bytes) => {
                Json(json!({"content_length":bytes.len(),"e_tag":"mock"})).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
    async fn put(
        State(mock): State<Mock>,
        Query(query): Query<HashMap<String, String>>,
        bytes: Bytes,
    ) -> Json<Value> {
        mock.objects
            .lock()
            .unwrap()
            .insert(query["key"].clone(), bytes.to_vec());
        mock.uploads.fetch_add(1, Ordering::SeqCst);
        Json(json!({"e_tag":"mock"}))
    }

    #[tokio::test]
    #[ignore = "requires FFPROBE_BIN"]
    async fn spool_survives_s3_outages() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("fixture-session");
        fs::create_dir(&session).unwrap();
        let path = session.join(format!(
            "segment-{}.part",
            OffsetDateTime::now_utc().unix_timestamp()
        ));
        fs::write(&path, include_bytes!("../tests/fixtures/blue.mp4")).unwrap();
        // 録画確定にDBは不要。再起動後も同じmanifestを使う。
        finish(&path, "camera").await.unwrap();
        recover(root.path(), "camera").await.unwrap();
        let manifest = path.with_extension("json");
        let segment: Segment = serde_json::from_reader(File::open(&manifest).unwrap()).unwrap();
        assert!(segment.size > 0);
        assert_eq!(segment.checksum.len(), 64);
        assert!(path.with_extension("mp4").exists());
        let vars = HashMap::from([
            ("RTSP_URL".into(), "rtsp://unused".into()),
            ("API_HUB_BASE_URL".into(), "http://127.0.0.1:1".into()),
        ]);
        let mut config = AppConfig::from_map(&vars).unwrap();
        let mock = Mock::default();
        mock.unavailable.store(true, Ordering::SeqCst);
        let app = Router::new()
            .route("/s3/head_object", post(head))
            .route("/s3/put_object_stream", post(put))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.api_hub_base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        assert!(upload_one(root.path(), &config, &http).await.unwrap());
        assert!(manifest.exists());
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 0);
        mock.unavailable.store(false, Ordering::SeqCst);
        let mut retry = read_segment(&manifest).unwrap();
        assert!(retry.next_attempt_at > 0);
        retry.next_attempt_at = 0;
        atomic_json(&manifest, &retry).unwrap();
        assert!(upload_one(root.path(), &config, &http).await.unwrap());
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 1);
        assert!(!manifest.exists());
        assert!(!path.with_extension("mp4").exists());
        // S3成功後ローカル解放前で停止しても、同じkeyを照合して転送を繰り返さない。
        let next = session.join(format!(
            "segment-{}.part",
            OffsetDateTime::now_utc().unix_timestamp() + 10
        ));
        fs::write(&next, include_bytes!("../tests/fixtures/blue.mp4")).unwrap();
        finish(&next, "camera").await.unwrap();
        let segment: Segment =
            serde_json::from_reader(File::open(next.with_extension("json")).unwrap()).unwrap();
        mock.objects.lock().unwrap().insert(
            segment.object_key,
            include_bytes!("../tests/fixtures/blue.mp4").to_vec(),
        );
        assert!(upload_one(root.path(), &config, &http).await.unwrap());
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 1);
        fs::write(session.join("segment-1.part"), b"interrupted").unwrap();
        recover(root.path(), "camera").await.unwrap();
        assert!(session.join("segment-1.corrupt").exists());
        assert_eq!(stats(root.path()).unwrap().2, 1);
        server.abort();
    }
    fn fixture(session: &Path, epoch: i64) -> PathBuf {
        let file = session.join(format!("segment-{epoch}.mp4"));
        fs::write(&file, b"video fixture").unwrap();
        let path = file.with_extension("json");
        let start = OffsetDateTime::from_unix_timestamp(epoch).unwrap();
        atomic_json(
            &path,
            &Segment {
                id: format!("fixture-{epoch}"),
                camera_id: "camera".into(),
                actual_start_at: start.format(&Rfc3339).unwrap(),
                actual_end_at: (start + time::Duration::seconds(1))
                    .format(&Rfc3339)
                    .unwrap(),
                file_name: file.file_name().unwrap().to_str().unwrap().into(),
                object_key: format!("fixed/{epoch}.mp4"),
                size: file.metadata().unwrap().len(),
                checksum: checksum(&file).unwrap(),
                attempts: 0,
                next_attempt_at: 0,
            },
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn invalid_manifests_do_not_block_upload_or_disappear_on_recovery() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session");
        fs::create_dir(&session).unwrap();
        let malformed = fixture(&session, 100);
        fs::write(&malformed, b"{broken").unwrap();
        let missing_field = fixture(&session, 101);
        fs::write(&missing_field, b"{}").unwrap();
        let invalid_time = fixture(&session, 102);
        let mut value: Value = serde_json::from_reader(File::open(&invalid_time).unwrap()).unwrap();
        value["actual_start_at"] = json!("not-a-date");
        atomic_json(&invalid_time, &value).unwrap();
        let normal = fixture(&session, 103);
        // 旧manifestの追加フィールド省略を確認。
        let mut legacy: Value = serde_json::from_reader(File::open(&normal).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("attempts");
        legacy.as_object_mut().unwrap().remove("next_attempt_at");
        atomic_json(&normal, &legacy).unwrap();
        let before = stats(root.path()).unwrap();
        assert_eq!(before.3, 3);
        assert!(before.0 > 0 && before.1 > 0.0);
        let mock = Mock::default();
        let app = Router::new()
            .route("/s3/head_object", post(head))
            .route("/s3/put_object_stream", post(put))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let vars = HashMap::from([
            ("RTSP_URL".into(), "rtsp://unused".into()),
            (
                "API_HUB_BASE_URL".into(),
                format!("http://{}", listener.local_addr().unwrap()),
            ),
        ]);
        let config = AppConfig::from_map(&vars).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::new();
        for _ in 0..2 {
            recover(root.path(), "camera").await.unwrap();
            upload_one(root.path(), &config, &http).await.unwrap();
            for path in [&malformed, &missing_field, &invalid_time] {
                assert!(path.exists() && path.with_extension("mp4").exists());
            }
            assert!(!normal.exists() && !normal.with_extension("mp4").exists());
            assert_eq!(stats(root.path()).unwrap().3, 3);
        }
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 1);
        // manifestだけが残った削除途中はS3を照合し、再PUTせず解放。
        let half_deleted = fixture(&session, 104);
        let segment = read_segment(&half_deleted).unwrap();
        mock.objects
            .lock()
            .unwrap()
            .insert(segment.object_key, b"video fixture".to_vec());
        fs::remove_file(half_deleted.with_extension("mp4")).unwrap();
        upload_one(root.path(), &config, &http).await.unwrap();
        assert!(!half_deleted.exists());
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 1);
        // 両方欠損、remoteサイズ不一致、checksum不一致は保持。他の転送は進める。
        let lost = fixture(&session, 105);
        fs::remove_file(lost.with_extension("mp4")).unwrap();
        let mismatch = fixture(&session, 106);
        mock.objects
            .lock()
            .unwrap()
            .insert("fixed/106.mp4".into(), vec![0]);
        let changed = fixture(&session, 107);
        fs::write(changed.with_extension("mp4"), b"changed").unwrap();
        let good = fixture(&session, 108);
        upload_one(root.path(), &config, &http).await.unwrap();
        assert!(lost.exists() && mismatch.exists() && changed.exists());
        assert!(!good.exists());
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 2);
        for path in [&malformed, &missing_field, &invalid_time] {
            fs::remove_file(path).unwrap();
        }
        assert_eq!(stats(root.path()).unwrap().3, 0);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires FFMPEG_BIN"]
    async fn stalled_rtsp_connection_times_out_on_each_reconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("rtsp://{}/fixture", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                connections.push(listener.accept().await.unwrap());
            }
        });
        for _ in 0..2 {
            let mut command = rtsp_command(&url, 500_000);
            command
                .args(["-f", "null", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let status = tokio::time::timeout(Duration::from_secs(8), command.status())
                .await
                .unwrap()
                .unwrap();
            assert!(!status.success());
        }
        server.abort();
    }
}
