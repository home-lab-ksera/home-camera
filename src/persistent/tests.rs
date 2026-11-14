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
fn closed_list_waits_for_newline_and_rejects_reused_names() {
    let root = tempfile::tempdir().unwrap();
    let csv = root.path().join("segments.csv");
    fs::write(
        &csv,
        "segment-100-000000000.part,0.000000,10.000000\nsegment-100-000000001.part,10.000000,20",
    )
    .unwrap();
    assert_eq!(closed_segments(root.path()).unwrap().len(), 1);
    fs::write(
        &csv,
        "segment-100.part,0.000000,5909.261979\nsegment-100.part,5909.262370,7201.281315\n",
    )
    .unwrap();
    assert!(closed_segments(root.path()).is_err());
    fs::write(&csv, "../segment-100.part,0,1\n").unwrap();
    assert!(closed_segments(root.path()).is_err());
}

#[test]
fn numbered_segments_keep_timeline_and_legacy_names() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("segments.csv"), "segment-100-000000000.part,0.000000,10.250000\nsegment-100-000000001.part,10.250000,20.500000\n").unwrap();
    for (name, expected) in [
        ("segment-100.part", 100000),
        ("segment-100-000000000.part", 100000),
        ("segment-100-000000001.mp4", 110250),
        ("segment-100-000000002.part", 120500),
    ] {
        let (start, _) = segment_start(&root.path().join(name)).unwrap();
        assert_eq!(start.unix_timestamp_nanos() / 1_000_000, expected);
    }
    assert!(segment_start(&root.path().join("segment-100-000000003.part")).is_err());
}

#[test]
fn ffmpeg_diagnostics_hide_rtsp_credentials() {
    let url = "rtsp://camera:secret-pass@192.0.2.1/stream";
    let output = redact_ffmpeg(&format!("Could not open {url}; password secret-pass"), url);
    assert!(!output.contains("secret-pass"));
    assert!(!output.contains(url));
    assert!(output.contains("Could not open"));
}

#[tokio::test]
async fn reviewed_quarantine_is_preserved_outside_the_active_queue() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join(".quarantine");
    fs::create_dir(&archive).unwrap();
    let file = archive.join("segment-100.corrupt");
    fs::write(&file, b"preserved evidence").unwrap();
    fs::write(archive.join("incident.json"), b"{\"reviewed\":true}").unwrap();
    assert!(manifests(root.path()).unwrap().is_empty());
    assert_eq!(recover(root.path(), "camera").await.unwrap(), 0);
    assert_eq!(stats(root.path()).unwrap().2, 0);
    assert_eq!(fs::read(file).unwrap(), b"preserved evidence");
}

#[tokio::test]
#[ignore = "requires FFMPEG_BIN and FFPROBE_BIN"]
async fn buffered_segments_have_unique_files_and_recover_after_stop() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("burst");
    fs::create_dir(&session).unwrap();
    let input = root.path().join("input.mp4");
    fs::write(&input, include_bytes!("../../tests/fixtures/blue.mp4")).unwrap();
    let mut command = Command::new(env::var("FFMPEG_BIN").expect("FFMPEG_BIN required"));
    // 実時間待ちなしで複数境界を通過させる。旧strftime名では全行が同じ名前になる。
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-stream_loop",
            "20",
            "-i",
        ])
        .arg(input)
        .args(["-map", "0:v:0", "-c", "copy", "-t", "12"]);
    segment_output(&mut command, &session, 1793560479, 1);
    let output = command.kill_on_drop(true).output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = closed_segments(&session).unwrap();
    assert!(rows.len() >= 6);
    for row in &rows {
        assert!(session.join(&row.name).exists());
    }
    assert_eq!(
        recover(root.path(), "camera").await.unwrap() as usize,
        rows.len()
    );
    let files = manifests(root.path()).unwrap();
    assert_eq!(files.len(), rows.len());
    let mut keys = HashSet::new();
    for file in files {
        let segment = read_segment(&file).unwrap();
        assert!(keys.insert(segment.object_key));
        assert_eq!(
            checksum(&file.with_extension("mp4")).unwrap(),
            segment.checksum
        );
    }
    assert_eq!(stats(root.path()).unwrap().2, 0);
    assert_eq!(recover(root.path(), "camera").await.unwrap(), 0);
}

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
    fs::write(&input, include_bytes!("../../tests/fixtures/blue.mp4")).unwrap();
    let mut command = Command::new(env::var("FFMPEG_BIN").expect("FFMPEG_BIN required"));
    command
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
        .args(["-map", "0:v:0", "-c", "copy"]);
    segment_output(
        &mut command,
        &session,
        OffsetDateTime::now_utc().unix_timestamp(),
        1,
    );
    let mut child = command
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
        Some(bytes) => Json(json!({"content_length":bytes.len(),"e_tag":"mock"})).into_response(),
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
    fs::write(&path, include_bytes!("../../tests/fixtures/blue.mp4")).unwrap();
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
    fs::write(&next, include_bytes!("../../tests/fixtures/blue.mp4")).unwrap();
    finish(&next, "camera").await.unwrap();
    let segment: Segment =
        serde_json::from_reader(File::open(next.with_extension("json")).unwrap()).unwrap();
    mock.objects.lock().unwrap().insert(
        segment.object_key,
        include_bytes!("../../tests/fixtures/blue.mp4").to_vec(),
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
