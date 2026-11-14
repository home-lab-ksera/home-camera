//! FFmpeg による録画と、録画状態のメトリクス。

use super::*;

#[derive(Default)]
pub(super) struct RecordingProgress {
    // 録画開始の猶予、または最後に正常確定したsegmentからの経過時間。
    // FFmpegの再起動では更新しない。
    pub(super) since: Option<Instant>,
}

impl RecordingProgress {
    pub(super) fn observe(&mut self, active_time: bool, blocked: bool, now: Instant) {
        if active_time && !blocked {
            self.since.get_or_insert(now);
        } else {
            self.since = None;
        }
    }

    pub(super) fn completed(&mut self, now: Instant) {
        if self.since.is_some() {
            self.since = Some(now);
        }
    }

    pub(super) fn stalled(&self, now: Instant, deadline: Duration) -> bool {
        self.since
            .is_some_and(|since| now.duration_since(since) > deadline)
    }
}

#[derive(Default)]
pub(super) struct Metrics {
    pub(super) bytes: u64,
    pub(super) oldest_age: f64,
    pub(super) blocked: bool,
    pub(super) active: bool,
    pub(super) partial: u64,
    pub(super) invalid_manifests: u64,
    pub(super) stalled: bool,
    pub(super) restarts: u64,
    pub(super) heartbeat: Option<Instant>,
    pub(super) stats_success: bool,
}
pub(super) fn allowed(now: OffsetDateTime) -> bool {
    let hour = now.to_offset(UtcOffset::from_hms(9, 0, 0).unwrap()).hour();
    !(9..19).contains(&hour)
}
pub(super) async fn stop(child: &mut Child) {
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

pub(super) fn rtsp_command(url: &str, timeout_us: u64) -> Command {
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

pub(super) fn segment_output(command: &mut Command, session: &Path, epoch: i64, seconds: u64) {
    command
        .args([
            "-f",
            "segment",
            "-segment_time",
            &seconds.to_string(),
            "-reset_timestamps",
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
        // 秒単位のstrftime名は、入力時刻の跳躍・バッファ放出で衝突する。
        .arg(session.join(format!("segment-{epoch}-%09d.part")));
}

pub(super) fn redact_ffmpeg(line: &str, rtsp_url: &str) -> String {
    let mut line = line.replace(rtsp_url, "[RTSP URL]");
    if let Ok(url) = reqwest::Url::parse(rtsp_url)
        && let Some(password) = url.password().filter(|value| !value.is_empty())
    {
        line = line.replace(password, "[REDACTED]");
    }
    line.chars().take(2048).collect()
}

pub(super) async fn recorder() -> Result<()> {
    let config = AppConfig::from_env()?;
    let root = root();
    let _lock = lock(&root, "recorder.lock")?;
    recover(&root, &config.camera_id).await?;
    let seconds: u64 = env::var("SEGMENT_SECONDS")
        .unwrap_or_else(|_| "3600".into())
        .parse()?;
    if !(10..=3600).contains(&seconds) {
        bail!("SEGMENT_SECONDS must be 10..3600");
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
            segment_output(
                &mut command,
                &session,
                OffsetDateTime::now_utc().unix_timestamp(),
                seconds,
            );
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut process = command.spawn().context("could not start ffmpeg")?;
            let stderr = process.stderr.take().context("ffmpeg stderr missing")?;
            let rtsp_url = config.rtsp_url.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("camera ffmpeg: {}", redact_ffmpeg(&line, &rtsp_url));
                }
            });
            child = Some((process, session));
            process_progress = Instant::now();
        }
        if let Some((process, session)) = &mut child {
            // muxerが閉じたファイルだけをCSVから確定する。現在書き込み中のpartは送らない。
            match closed_segments(session) {
                Ok(rows) => {
                    for row in rows {
                        let path = session.join(row.name);
                        if path.exists() && path.extension().is_some_and(|v| v == "part") {
                            match finish(&path, &config.camera_id).await {
                                Ok(()) => {
                                    process_progress = Instant::now();
                                    recording_progress.completed(process_progress);
                                }
                                Err(error) => {
                                    eprintln!(
                                        "closed segment {} could not be finalized: {error}",
                                        path.display()
                                    );
                                    if path.exists() {
                                        fs::rename(&path, path.with_extension("corrupt"))?;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => eprintln!("camera closed segment list rejected: {error}"),
            }
            let status = process.try_wait()?;
            let exited = status.is_some();
            // 接続猶予60秒とkeyframe境界の余裕を含め、完成segmentの停止を検知する。
            let stalled = process_progress.elapsed() > progress_deadline;
            if exited || stalled || !active_time || blocked {
                if exited || stalled {
                    eprintln!(
                        "camera ffmpeg reconnect: exit={status:?}, stalled={stalled}, session={}",
                        session.display()
                    );
                }
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
