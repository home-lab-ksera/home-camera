//! PVC 上の session・segment・manifest の読み書きと復旧。

use super::*;

#[derive(Serialize, Deserialize, Debug)]
pub(super) struct Segment {
    pub(super) id: String,
    pub(super) camera_id: String,
    pub(super) actual_start_at: String,
    pub(super) actual_end_at: String,
    pub(super) file_name: String,
    pub(super) object_key: String,
    pub(super) size: u64,
    pub(super) checksum: String,
    #[serde(default)]
    pub(super) attempts: u32,
    #[serde(default)]
    pub(super) next_attempt_at: i64,
}

pub(super) fn read_segment(path: &Path) -> Result<Segment> {
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

pub(super) fn root() -> PathBuf {
    env::var_os("SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/spool"))
}
pub(super) fn lock(root: &Path, name: &str) -> Result<File> {
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
pub(super) fn checksum(path: &Path) -> Result<String> {
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
pub(super) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut file = File::create(&temp)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    File::open(path.parent().context("manifest directory missing")?)?.sync_all()?;
    Ok(())
}
pub(super) fn sessions(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        // 調査済みの原本保全用。自動で隔離・削除・再送はしない。
        .filter(|entry| entry.file_name() != ".quarantine")
        .map(|entry| entry.path())
        .collect())
}
pub(super) fn manifests(root: &Path) -> Result<Vec<PathBuf>> {
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

pub(super) struct ClosedSegment {
    pub(super) name: String,
    pub(super) start: f64,
    pub(super) end: f64,
}

// CSVの末尾はFFmpegが追記中かもしれない。改行まで書かれた行だけを扱う。
pub(super) fn closed_segments(session: &Path) -> Result<Vec<ClosedSegment>> {
    let Some(csv) = present(fs::read_to_string(session.join("segments.csv")))? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    let mut names = HashSet::new();
    for line in csv
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
    {
        let fields: Vec<_> = line.trim().split(',').collect();
        if fields.len() != 3 {
            bail!("invalid closed segment CSV row");
        }
        let name = fields[0].trim_matches('"');
        if !name.starts_with("segment-")
            || !name.ends_with(".part")
            || Path::new(name).file_name().and_then(|v| v.to_str()) != Some(name)
            || !names.insert(name.to_owned())
        {
            bail!("unsafe or repeated closed segment filename");
        }
        let start = fields[1].parse::<f64>()?;
        let end = fields[2].parse::<f64>()?;
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end < start || end > 86400.0 {
            bail!("invalid closed segment timestamps");
        }
        result.push(ClosedSegment {
            name: name.into(),
            start,
            end,
        });
    }
    Ok(result)
}

pub(super) fn segment_start(path: &Path) -> Result<(OffsetDateTime, String)> {
    let stem = path
        .file_stem()
        .context("segment filename missing")?
        .to_str()
        .context("invalid segment filename")?;
    let identity = stem
        .strip_prefix("segment-")
        .context("invalid segment filename")?;
    let (epoch, sequence) = match identity.split_once('-') {
        Some((epoch, sequence)) => (epoch.parse::<i64>()?, Some(sequence.parse::<usize>()?)),
        None => (identity.parse::<i64>()?, None), // 既存spoolの時刻だけの名前も回収できる。
    };
    let mut start = OffsetDateTime::from_unix_timestamp(epoch)?;
    if let Some(sequence) = sequence {
        let rows = closed_segments(path.parent().context("session directory missing")?)?;
        let offset = if let Some(row) = rows.get(sequence) {
            if row.name != format!("{stem}.part") {
                bail!("segment sequence does not match CSV");
            }
            row.start
        } else if sequence == rows.len() {
            // 停止時にCSVへ登録されなかった最後のpart。ffprobe成功時だけ回収する。
            rows.last().map_or(0.0, |row| row.end)
        } else {
            bail!("segment start timestamp missing");
        };
        start += time::Duration::milliseconds((offset * 1000.0) as i64);
    }
    Ok((start, identity.to_owned()))
}

pub(super) async fn finish(path: &Path, camera: &str) -> Result<()> {
    let (start, identity) = segment_start(path)?;
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
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !probe.status.success() {
        bail!(
            "segment is incomplete: {}",
            String::from_utf8_lossy(&probe.stderr)
                .chars()
                .take(2048)
                .collect::<String>()
        );
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
        id: format!("{session}-{identity}"),
        camera_id: camera.clone(),
        actual_start_at: start.format(&Rfc3339)?,
        actual_end_at: end.format(&Rfc3339)?,
        file_name: mp4.file_name().unwrap().to_str().unwrap().into(),
        object_key: format!(
            "{camera}/{:04}/{:02}/{:02}/{}-{session}{}.mp4",
            start.year(),
            start.month() as u8,
            start.day(),
            start.unix_timestamp(),
            identity
                .split_once('-')
                .map_or(String::new(), |(_, sequence)| format!("-{sequence}")),
        ),
        size: metadata.len(),
        checksum: checksum(&mp4)?,
        attempts: 0,
        next_attempt_at: 0,
    };
    atomic_json(&mp4.with_extension("json"), &segment)?;
    Ok(())
}

pub(super) async fn recover(root: &Path, camera: &str) -> Result<u64> {
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

// uploaderは独立して送信済みファイル・sessionを削除する。走査後の消失は正常。
pub(super) fn present<T>(result: std::io::Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub(super) fn stats(root: &Path) -> Result<(u64, f64, u64, u64)> {
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
