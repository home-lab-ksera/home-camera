//! 録画はDBに依存せずPVCへ確定し、別プロセスがDBとS3へ転送する。
use crate::{config::AppConfig, key::sanitize_camera_id};
use anyhow::{Context, Result, bail};
use axum::{Router, extract::State, http::StatusCode, routing::get};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    ConnectOptions, PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};
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
}

async fn connect() -> Result<PgPool> {
    let options = if let Ok(url) = env::var("DATABASE_URL") {
        url.parse::<PgConnectOptions>()?
    } else {
        PgConnectOptions::new()
            .host(&env::var("PGHOST")?)
            .username("app")
            .database(&env::var("PGDATABASE")?)
            .password(&env::var("PGPASSWORD")?)
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert(env::var("PGSSLROOTCERT")?)
    };
    let max: u32 = env::var("PGPOOL_MAX")
        .unwrap_or_else(|_| "2".into())
        .parse()?;
    if !(1..=2).contains(&max) {
        bail!("PGPOOL_MAX must be 1..2");
    }
    Ok(PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(60))
        .connect_lazy_with(
            options
                .application_name("camera-uploader")
                .disable_statement_logging(),
        ))
}
pub async fn dispatch() -> Result<bool> {
    let mode = env::args().nth(1).unwrap_or_default();
    if mode.is_empty() {
        return Ok(false);
    }
    match mode.as_str() {
        "--migrate" => {
            sqlx::migrate!("./migrations")
                .run(&connect().await?)
                .await?;
        }
        "--record" => recorder().await?,
        "--upload" => uploader().await?,
        _ => bail!("Usage: home-camera [--migrate|--record|--upload]"),
    }
    Ok(true)
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
    };
    atomic_json(&mp4.with_extension("json"), &segment)?;
    Ok(())
}

async fn recover(root: &Path, camera: &str) -> Result<()> {
    for session in sessions(root)? {
        for entry in fs::read_dir(&session)? {
            let path = entry?.path();
            if !path.extension().is_some_and(|v| v == "part" || v == "mp4")
                || path.with_extension("json").exists()
            {
                continue;
            }
            if let Err(error) = finish(&path, camera).await {
                eprintln!("camera segment could not be recovered: {error}");
                if path.extension().is_some_and(|v| v == "part") {
                    fs::rename(&path, path.with_extension("corrupt"))?;
                }
            }
        }
        if session.join(".active").exists() {
            fs::remove_file(session.join(".active"))?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Metrics {
    bytes: u64,
    oldest_age: f64,
    blocked: bool,
    active: bool,
    partial: u64,
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
fn stats(root: &Path) -> Result<(u64, f64, u64)> {
    let mut bytes = 0;
    let mut oldest = OffsetDateTime::now_utc().unix_timestamp();
    let mut partial = 0;
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
                let Some(file) = present(File::open(&path))? else {
                    continue;
                };
                if let Ok(segment) = serde_json::from_reader::<_, Segment>(file) {
                    oldest = oldest.min(
                        OffsetDateTime::parse(&segment.actual_start_at, &Rfc3339)?.unix_timestamp(),
                    );
                }
            }
        }
    }
    Ok((
        bytes,
        (OffsetDateTime::now_utc().unix_timestamp() - oldest).max(0) as f64,
        partial,
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
        .route("/metrics",get(|State(value):State<Arc<Mutex<Metrics>>>|async move{let v=value.lock().unwrap();format!("home_camera_spool_bytes {}\nhome_camera_spool_oldest_age_seconds {}\nhome_camera_spool_blocked {}\nhome_camera_recording_active {}\nhome_camera_partial_segments {}\nhome_camera_recorder_restarts_total {}\nhome_camera_spool_stats_success {}\n",v.bytes,v.oldest_age,u8::from(v.blocked),u8::from(v.active),v.partial,v.restarts,u8::from(v.stats_success))})).with_state(metrics.clone());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:9090").await?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut child: Option<(Child, PathBuf)> = None;
    let mut next_start = Instant::now();
    let mut sampled = (0, 0.0, 0);
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
        let (bytes, age, partial) = sampled;
        let blocked = bytes >= max || fs2::available_space(&root)? < 2 * 1024 * 1024 * 1024;
        let active_time = allowed(OffsetDateTime::now_utc());
        {
            let mut v = metrics.lock().unwrap();
            v.bytes = bytes;
            v.oldest_age = age;
            v.partial = partial;
            v.blocked = blocked;
            v.active = child.is_some();
            v.heartbeat = Some(Instant::now());
        }
        if child.is_none() && active_time && !blocked && Instant::now() >= next_start {
            let session = root.join(uuid::Uuid::new_v4().to_string());
            fs::create_dir(&session)?;
            File::create(session.join(".active"))?;
            let mut command = Command::new("ffmpeg");
            command.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-rtsp_transport",
                "tcp",
                "-rw_timeout",
                "30000000",
                "-i",
                &config.rtsp_url,
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
        }
        if let Some((process, session)) = &mut child {
            // muxerが閉じたファイルだけをCSVから確定する。現在書き込み中のpartは送らない。
            if let Ok(csv) = fs::read_to_string(session.join("segments.csv")) {
                for line in csv.lines() {
                    if let Some(name) = line.split(',').next() {
                        let path = session.join(name.trim_matches('"'));
                        if path.exists() && path.extension().is_some_and(|v| v == "part") {
                            if let Err(error) = finish(&path, &config.camera_id).await {
                                eprintln!("closed segment could not be finalized: {error}");
                                fs::rename(&path, path.with_extension("corrupt"))?;
                            }
                        }
                    }
                }
            }
            let exited = process.try_wait()?.is_some();
            if exited || !active_time || blocked {
                stop(process).await;
                child = None;
                recover(&root, &config.camera_id).await?;
                next_start = Instant::now() + Duration::from_secs(10);
                metrics.lock().unwrap().restarts += u64::from(exited);
            }
        }
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
    let pool = connect().await?;
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
        if let Err(error) = upload_one(&root, &pool, &config, &http).await {
            eprintln!("camera upload retry: {error:#}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn upload_one(
    root: &Path,
    pool: &PgPool,
    config: &AppConfig,
    http: &reqwest::Client,
) -> Result<bool> {
    let mut pending = Vec::new();
    let mut records = Vec::new();
    for path in manifests(root)? {
        let segment: Segment = serde_json::from_reader(File::open(&path)?)?;
        let file = path.parent().unwrap().join(&segment.file_name);
        records.push(json!({"id":segment.id,"camera_id":segment.camera_id,"started":segment.actual_start_at,"ended":segment.actual_end_at,"path":file.to_str().context("invalid spool path")?,"key":segment.object_key,"size":segment.size,"checksum":segment.checksum}));
        pending.push((segment.id, path, file));
    }
    if !records.is_empty() {
        // 滞留中の同じ行を5秒ごとにUPDATEしてWALを増やさず、一括登録・照合する。
        sqlx::query("INSERT INTO recordings(id,camera_id,actual_start_at,actual_end_at,spool_path,object_key,size,checksum) SELECT r.id,r.camera_id,r.started::timestamptz,r.ended::timestamptz,r.path,r.key,r.size,r.checksum FROM jsonb_to_recordset($1) AS r(id text,camera_id text,started text,ended text,path text,key text,size bigint,checksum text) ON CONFLICT(id) DO NOTHING")
            .bind(json!(records)).execute(pool).await?;
        let ids: Vec<_> = pending.iter().map(|(id, _, _)| id.clone()).collect();
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT id FROM recordings WHERE id=ANY($1) AND state='stored'")
                .bind(ids)
                .fetch_all(pool)
                .await?;
        for (id, path, file) in pending {
            if stored.contains(&id) {
                if file.exists() {
                    fs::remove_file(file)?;
                }
                fs::remove_file(path)?;
            }
        }
    }
    // 完了済みsessionの小さなCSVも残し続けない。稼働中・作成直後のdirectoryは触らない。
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
    sqlx::query("UPDATE recordings SET state='retry',next_attempt_at=now(),last_error='upload lease expired' WHERE state='uploading' AND lease_until<now()").execute(pool).await?;
    let row=sqlx::query("WITH candidate AS(SELECT id FROM recordings WHERE state IN ('pending','retry') AND next_attempt_at<=now() ORDER BY actual_start_at FOR UPDATE SKIP LOCKED LIMIT 1) UPDATE recordings r SET state='uploading',attempts=attempts+1,claim_token=gen_random_uuid(),lease_until=now()+interval '5 minutes' FROM candidate c WHERE r.id=c.id RETURNING r.* ,r.claim_token::text AS token").fetch_optional(pool).await?;
    let Some(row) = row else { return Ok(false) };
    let id: String = row.get("id");
    let token: String = row.get("token");
    let key: String = row.get("object_key");
    let path = PathBuf::from(row.get::<String, _>("spool_path"));
    let size: i64 = row.get("size");
    let transfer = async {
        if path.metadata()?.len() != size as u64
            || checksum(&path)? != row.get::<String, _>("checksum")
        {
            bail!("spool file changed; preserving file for inspection");
        }
        let metadata = |response: Value| -> Result<Option<String>> {
            if response["content_length"].as_i64() != Some(size) {
                bail!("stored recording size mismatch");
            }
            Ok(response["e_tag"].as_str().map(str::to_owned))
        };
        let head = http
            .post(format!("{}/s3/head_object", config.api_hub_base_url))
            .json(&json!({"bucket":config.s3_bucket,"key":key}))
            .send()
            .await?;
        if head.status().is_success() {
            return metadata(head.json().await?);
        }
        if head.status() != reqwest::StatusCode::NOT_FOUND {
            bail!("recording HEAD failed: {}", head.status());
        }
        crate::storage::upload_file(config, &key, &path).await?;
        let head = http
            .post(format!("{}/s3/head_object", config.api_hub_base_url))
            .json(&json!({"bucket":config.s3_bucket,"key":key}))
            .send()
            .await?
            .error_for_status()?;
        metadata(head.json().await?)
    };
    tokio::pin!(transfer);
    let result = loop {
        tokio::select! {result=&mut transfer=>break result,_=tokio::time::sleep(Duration::from_secs(60))=>{
            let update=sqlx::query("UPDATE recordings SET lease_until=now()+interval '5 minutes' WHERE id=$1 AND claim_token=$2::uuid AND state='uploading'").bind(&id).bind(&token).execute(pool).await?;
            if update.rows_affected()!=1{bail!("upload lease lost");}
        }}
    };
    match result {
        Ok(etag) => {
            let update=sqlx::query("UPDATE recordings SET state='stored',stored_at=now(),etag=$3,lease_until=NULL,last_error=NULL WHERE id=$1 AND claim_token=$2::uuid AND state='uploading'").bind(&id).bind(&token).bind(etag).execute(pool).await?;
            if update.rows_affected() == 1 {
                fs::remove_file(&path)?;
                fs::remove_file(path.with_extension("json"))?;
            }
        }
        Err(error) => {
            sqlx::query("UPDATE recordings SET state='retry',next_attempt_at=now()+make_interval(secs=>LEAST(900,10*power(2,LEAST(attempts,7)))::double precision),lease_until=NULL,last_error=$3 WHERE id=$1 AND claim_token=$2::uuid AND state='uploading'").bind(&id).bind(&token).bind(format!("{error:#}").chars().take(2000).collect::<String>()).execute(pool).await?;
        }
    }
    Ok(true)
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
    #[ignore = "requires FFPROBE_BIN and isolated TEST_DATABASE_URL"]
    async fn spool_survives_database_and_s3_outages() {
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
        let unavailable = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(150))
            .connect_lazy("postgres://127.0.0.1:1/no_database")
            .unwrap();
        let vars = HashMap::from([
            ("RTSP_URL".into(), "rtsp://unused".into()),
            ("API_HUB_BASE_URL".into(), "http://127.0.0.1:1".into()),
        ]);
        let mut config = AppConfig::from_map(&vars).unwrap();
        assert!(
            upload_one(root.path(), &unavailable, &config, &reqwest::Client::new())
                .await
                .is_err()
        );
        assert!(manifest.exists());
        assert!(path.with_extension("mp4").exists());

        let url = env::var("TEST_DATABASE_URL").expect("isolated database required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("camera_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = format!("SET search_path TO {schema}");
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |connection, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&search).execute(connection).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
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
        assert!(
            upload_one(root.path(), &pool, &config, &http)
                .await
                .unwrap()
        );
        assert!(manifest.exists());
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 0);
        mock.unavailable.store(false, Ordering::SeqCst);
        sqlx::query("UPDATE recordings SET next_attempt_at=now()")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            upload_one(root.path(), &pool, &config, &http)
                .await
                .unwrap()
        );
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 1);
        assert!(!manifest.exists());
        assert!(!path.with_extension("mp4").exists());
        let state: String = sqlx::query_scalar("SELECT state FROM recordings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "stored");

        // S3成功後DB commit前で停止しても、同じkeyを照合して転送を繰り返さない。
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
        assert!(
            upload_one(root.path(), &pool, &config, &http)
                .await
                .unwrap()
        );
        assert_eq!(mock.uploads.load(Ordering::SeqCst), 1);
        fs::write(session.join("segment-1.part"), b"interrupted").unwrap();
        recover(root.path(), "camera").await.unwrap();
        assert!(session.join("segment-1.corrupt").exists());
        assert_eq!(stats(root.path()).unwrap().2, 1);
        server.abort();
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
    }
}
