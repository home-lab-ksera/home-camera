//! 録画から「動きがあった区間」を見つける。
//!
//! FFmpeg で低いフレームレート・低い解像度のグレースケールにデコードし、
//! 各フレームを1秒前のフレームと比べて、変化した画素の割合を求める。
//! 背景を平均で持つと、人が去ったあとも残像が残って区間が延びるため、使わない。
//! 誤検出を減らすため、割合がしきい値を一定時間以上続けて超えたときだけ区間にする。
//! 画面全体が一度に変わったもの（赤外線への切り替えや照明）は `Lighting` として分ける。
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    collections::VecDeque,
    env,
    io::Read,
    path::Path,
    process::{Command, Stdio},
};

/// 解析の設定。既定値は誤検出を減らす側に寄せている。
///
/// 2026-11 に実際の夜間録画3晩（1080p・15fps）で調整した。しきい値 0.01 では、
/// 夜明け（05〜07時）の明るさの変化が1晩に40件ほど区間になったが、0.03 ではほぼ消えた。
#[derive(Clone, Debug)]
pub struct Params {
    /// 1秒あたりに調べるフレーム数。
    pub fps: u32,
    pub width: usize,
    pub height: usize,
    /// 1秒前との差がこれを超えた画素を「変化した」とみなす（0〜255）。
    pub pixel_delta: u8,
    /// 変化した画素の割合がこれ以上なら、そのフレームを「動きあり」とみなす。
    pub threshold: f32,
    /// 画面のこの割合以上が一度に変わったら、照明の変化として扱う。
    pub lighting_fraction: f32,
    /// 「動きあり」がこの秒数以上続いたものだけを区間にする。
    pub min_seconds: f32,
    /// この秒数未満の途切れは、前後の区間をつなげる。
    pub merge_gap_seconds: f32,
    /// 解析から外す領域（画面に対する割合で x, y, 幅, 高さ）。時計表示などに使う。
    pub masks: Vec<[f32; 4]>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            fps: 2,
            width: 160,
            height: 90,
            pixel_delta: 20,
            threshold: 0.03,
            lighting_fraction: 0.5,
            min_seconds: 3.0,
            merge_gap_seconds: 60.0,
            masks: Vec::new(),
        }
    }
}

impl Params {
    /// `MOTION_*` 環境変数で既定値を上書きする。
    pub fn from_env() -> Result<Self> {
        let mut params = Self::default();
        if let Some(v) = var("MOTION_PIXEL_DELTA")? {
            params.pixel_delta = v.parse().context("MOTION_PIXEL_DELTA")?;
        }
        if let Some(v) = var("MOTION_THRESHOLD")? {
            params.threshold = v.parse().context("MOTION_THRESHOLD")?;
        }
        if let Some(v) = var("MOTION_MIN_SECONDS")? {
            params.min_seconds = v.parse().context("MOTION_MIN_SECONDS")?;
        }
        if let Some(v) = var("MOTION_MERGE_GAP_SECONDS")? {
            params.merge_gap_seconds = v.parse().context("MOTION_MERGE_GAP_SECONDS")?;
        }
        if let Some(v) = var("MOTION_MASKS")? {
            params.masks = parse_masks(&v)?;
        }
        params.validate()?;
        Ok(params)
    }

    fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.threshold) || self.threshold == 0.0 {
            bail!("MOTION_THRESHOLD must be in (0, 1]");
        }
        if self.min_seconds < 0.0 || self.merge_gap_seconds < 0.0 {
            bail!("MOTION_MIN_SECONDS and MOTION_MERGE_GAP_SECONDS must not be negative");
        }
        if self.fps == 0 || self.width == 0 || self.height == 0 {
            bail!("fps and frame size must be positive");
        }
        Ok(())
    }

    fn frame_len(&self) -> usize {
        self.width * self.height
    }
}

fn var(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(v) if v.trim().is_empty() => Ok(None),
        Ok(v) => Ok(Some(v)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(e).context(name.to_string()),
    }
}

/// `x,y,w,h;x,y,w,h` 形式（各値は 0〜1 の割合）を読む。
pub fn parse_masks(raw: &str) -> Result<Vec<[f32; 4]>> {
    raw.split(';')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let values: Vec<f32> = part
                .split(',')
                .map(|v| v.trim().parse::<f32>())
                .collect::<Result<_, _>>()
                .with_context(|| format!("invalid MOTION_MASKS entry: {part}"))?;
            let [x, y, w, h] = values[..] else {
                bail!("MOTION_MASKS entry needs x,y,w,h: {part}");
            };
            if [x, y, w, h].iter().any(|v| !(0.0..=1.0).contains(v)) || w == 0.0 || h == 0.0 {
                bail!("MOTION_MASKS values must be fractions in 0..1: {part}");
            }
            Ok([x, y, w, h])
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Motion,
    Lighting,
}

/// 見つかった区間。時刻は録画の先頭からの秒数。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub start: f32,
    pub end: f32,
    /// 区間内で最も大きかった「変化した画素の割合」。
    pub peak: f32,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub version: u32,
    pub analyzed_seconds: f32,
    pub events: Vec<Event>,
    /// 調整用。各フレームの「変化した画素の割合」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scores: Option<Vec<f32>>,
    /// 調整用。画素ごとに「前のフレームから変化した」回数。時計表示などの位置を探すのに使う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Vec<u32>>,
}

/// フレームを1枚ずつ受け取り、区間を求める。
pub struct Detector {
    params: Params,
    included: Vec<bool>,
    included_count: usize,
    /// 直近 `fps` 枚。先頭が1秒前のフレーム。
    history: VecDeque<Vec<u8>>,
    previous: Option<Vec<u8>>,
    activity: Vec<u32>,
    scores: Vec<f32>,
    lighting: Vec<bool>,
}

impl Detector {
    pub fn new(params: Params) -> Self {
        let included = mask_grid(&params);
        let included_count = included.iter().filter(|v| **v).count();
        let len = params.frame_len();
        Self {
            params,
            included,
            included_count,
            history: VecDeque::new(),
            previous: None,
            activity: vec![0; len],
            scores: Vec::new(),
            lighting: Vec::new(),
        }
    }

    pub fn push(&mut self, frame: &[u8]) {
        debug_assert_eq!(frame.len(), self.params.frame_len());
        let delta = self.params.pixel_delta;
        if let Some(previous) = &self.previous {
            for (count, (&now, &before)) in self.activity.iter_mut().zip(frame.iter().zip(previous))
            {
                if now.abs_diff(before) > delta {
                    *count += 1;
                }
            }
        }
        let score = match self.history.front() {
            Some(reference) if self.included_count > 0 => {
                let changed = frame
                    .iter()
                    .zip(reference)
                    .zip(&self.included)
                    .filter(|((now, before), included)| {
                        **included && now.abs_diff(**before) > delta
                    })
                    .count();
                changed as f32 / self.included_count as f32
            }
            _ => 0.0,
        };
        self.scores.push(score);
        self.lighting.push(score >= self.params.lighting_fraction);
        if self.history.len() == self.params.fps as usize {
            self.history.pop_front();
        }
        self.history.push_back(frame.to_vec());
        self.previous = Some(frame.to_vec());
    }

    pub fn finish(self, include_debug: bool) -> Report {
        let events = find_events(&self.scores, &self.lighting, &self.params);
        let analyzed_seconds = self.scores.len() as f32 / self.params.fps as f32;
        Report {
            version: 1,
            analyzed_seconds,
            events,
            scores: include_debug.then_some(self.scores),
            activity: include_debug.then_some(self.activity),
        }
    }
}

fn mask_grid(params: &Params) -> Vec<bool> {
    let (w, h) = (params.width, params.height);
    let mut included = vec![true; w * h];
    for [mx, my, mw, mh] in &params.masks {
        let x0 = (mx * w as f32).floor() as usize;
        let y0 = (my * h as f32).floor() as usize;
        let x1 = (((mx + mw) * w as f32).ceil() as usize).min(w);
        let y1 = (((my + mh) * h as f32).ceil() as usize).min(h);
        for y in y0..y1 {
            for x in x0..x1 {
                included[y * w + x] = false;
            }
        }
    }
    included
}

/// フレームごとの判定から区間を作る。
fn find_events(scores: &[f32], lighting: &[bool], params: &Params) -> Vec<Event> {
    let step = 1.0 / params.fps as f32;
    let merge_gap = (params.merge_gap_seconds * params.fps as f32).round() as usize;
    let min_active = (params.min_seconds * params.fps as f32).ceil() as usize;
    let mut events: Vec<Event> = Vec::new();
    // (開始, 終了, 種類, 判定が立っていたフレーム数)
    let mut runs: Vec<(usize, usize, EventKind, usize)> = Vec::new();
    let mut i = 0;
    while i < scores.len() {
        let Some(kind) = classify(scores[i], lighting[i], params) else {
            i += 1;
            continue;
        };
        let start = i;
        while i < scores.len() && classify(scores[i], lighting[i], params) == Some(kind) {
            i += 1;
        }
        let len = i - start;
        if kind == EventKind::Motion {
            // 1秒前と比べるので、1フレームだけの変化（圧縮ノイズや単発のスパイク）は、
            // そのフレームと1秒後のフレームで、離れた1フレームずつの判定になる。つなげる前に捨てる。
            if len < 2 {
                continue;
            }
            // 動きが止まったあとも、1秒間は「1秒前」との差が残る。その分を長さから引く。
            let lag = params.fps as usize;
            runs.push((
                start,
                i - lag.min(len - 1),
                kind,
                len.saturating_sub(lag).max(1),
            ));
        } else {
            runs.push((start, i, kind, len));
        }
    }
    // 近い区間をつなげる。
    let mut merged: Vec<(usize, usize, EventKind, usize)> = Vec::new();
    for run in runs {
        match merged.last_mut() {
            Some(last) if last.2 == run.2 && run.0 - last.1 < merge_gap.max(1) => {
                last.1 = run.1;
                last.3 += run.3;
            }
            _ => merged.push(run),
        }
    }
    for (start, end, kind, active) in merged {
        // 短いものを捨てる。つなげた長さではなく、判定が立っていたフレーム数で数えるので、
        // 単発のスパイクがいくつ並んでも区間にはならない。
        // 照明の変化は一瞬で終わるので、この条件はかけない。
        if kind == EventKind::Motion && active < min_active {
            continue;
        }
        let peak = scores[start..end].iter().copied().fold(0.0, f32::max);
        events.push(Event {
            kind,
            start: start as f32 * step,
            end: end as f32 * step,
            peak,
        });
    }
    events
}

fn classify(score: f32, lighting: bool, params: &Params) -> Option<EventKind> {
    if lighting {
        Some(EventKind::Lighting)
    } else if score >= params.threshold {
        Some(EventKind::Motion)
    } else {
        None
    }
}

/// FFmpeg で録画をデコードして解析する。`FFMPEG_BIN` があればそれを使う。
pub fn analyze_file(path: &Path, params: &Params, include_debug: bool) -> Result<Report> {
    let ffmpeg = env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".into());
    let filter = format!(
        "fps={},scale={}:{}:flags=area,format=gray",
        params.fps, params.width, params.height
    );
    let mut child = Command::new(&ffmpeg)
        .args(["-nostdin", "-v", "error", "-threads", "1", "-i"])
        .arg(path)
        .args([
            "-an", "-sn", "-vf", &filter, "-f", "rawvideo", "-pix_fmt", "gray", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {ffmpeg}"))?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout")?;
    let mut detector = Detector::new(params.clone());
    let mut frame = vec![0u8; params.frame_len()];
    loop {
        match stdout.read_exact(&mut frame) {
            Ok(()) => detector.push(&frame),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading ffmpeg output"),
        }
    }
    let output = child.wait_with_output().context("waiting for ffmpeg")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg failed ({}): {}", output.status, stderr.trim());
    }
    Ok(detector.finish(include_debug))
}

#[cfg(test)]
mod tests;
