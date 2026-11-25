//! 前の晩の録画で「動きがあった区間」をまとめ、文字だけで Slack に送る（`--summary`）。
//!
//! 朝に CronJob で1回動かす。録画の横にある `<key>.motion.json`（uploader が置く）を api-hub 経由で読み、
//! api-hub の通知キューに入れる。同じ晩の通知は event_id が同じなので、再実行しても二重に送られない。
//! 自宅の映像は外部サービスに出さないので、画像は送らない。
use crate::motion::{EventKind, Report};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, env, time::Duration};
use time::{OffsetDateTime, Time, UtcOffset};

const JST: UtcOffset = match UtcOffset::from_hms(9, 0, 0) {
    Ok(offset) => offset,
    Err(_) => panic!("JST offset"),
};
/// 通知に並べる区間の上限。超えた分は件数だけを書く。
const MAX_LINES: usize = 30;

/// まとめる晩。`end`（JST 09:00）より前の24時間に始まった録画を対象にする。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Night {
    pub end: OffsetDateTime,
}

impl Night {
    /// `now` 以前で最も新しい JST 09:00 に終わる晩。
    pub fn ending_before(now: OffsetDateTime) -> Self {
        let local = now.to_offset(JST);
        let mut end = local.replace_time(Time::from_hms(9, 0, 0).expect("09:00"));
        if end > local {
            end -= time::Duration::days(1);
        }
        Self { end }
    }

    fn start(&self) -> OffsetDateTime {
        self.end - time::Duration::days(1)
    }

    fn contains(&self, epoch: i64) -> bool {
        (self.start().unix_timestamp()..self.end.unix_timestamp()).contains(&epoch)
    }

    /// 録画のキーは UTC の日付で分かれているので、晩にかかる UTC の日付のフォルダを全部見る。
    fn prefixes(&self, camera: &str) -> Vec<String> {
        let mut days = Vec::new();
        let mut day = self.start().to_offset(UtcOffset::UTC).date();
        let last = (self.end - time::Duration::seconds(1))
            .to_offset(UtcOffset::UTC)
            .date();
        while day <= last {
            days.push(format!(
                "{camera}/{:04}/{:02}/{:02}/",
                day.year(),
                day.month() as u8,
                day.day()
            ));
            day = day.next_day().expect("date in range");
        }
        days
    }

    pub fn event_id(&self) -> String {
        let day = self.end.date();
        format!(
            "motion-summary-{:04}-{:02}-{:02}",
            day.year(),
            day.month() as u8,
            day.day()
        )
    }
}

/// 録画1本と、その解析結果。
#[derive(Debug)]
pub struct Recording {
    pub file_name: String,
    pub start: i64,
    pub motion: Motion,
}

#[derive(Debug)]
pub enum Motion {
    Analyzed(Report),
    Failed,
    Missing,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Sidecar {
    Failed {
        #[allow(dead_code)]
        error: String,
    },
    Analyzed(Report),
}

fn clock(epoch: f64) -> String {
    let time = OffsetDateTime::from_unix_timestamp(epoch.floor() as i64)
        .map(|t| t.to_offset(JST))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:02}:{:02}:{:02}",
        time.hour(),
        time.minute(),
        time.second()
    )
}

fn offset(seconds: f32) -> String {
    let total = seconds.max(0.0).floor() as u32;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn duration(seconds: f32) -> String {
    let total = (seconds.round() as u32).max(1);
    match (total / 3600, total % 3600 / 60, total % 60) {
        (0, 0, s) => format!("{s}秒"),
        (0, m, 0) => format!("{m}分"),
        (0, m, s) => format!("{m}分{s}秒"),
        (h, m, _) => format!("{h}時間{m}分"),
    }
}

/// 通知の本文。動きの区間を時刻順に「時刻・長さ・ファイル名と位置」で並べる。
pub fn compose(night: &Night, recordings: &[Recording]) -> String {
    let start = night.end - time::Duration::hours(14);
    let header = format!(
        "昨夜のカメラ（{}/{} {:02}:00〜{}/{} {:02}:00）",
        start.month() as u8,
        start.day(),
        start.hour(),
        night.end.month() as u8,
        night.end.day(),
        night.end.hour(),
    );
    if recordings.is_empty() {
        return format!("{header}：録画がありません。");
    }
    let mut lines = Vec::new();
    for recording in recordings {
        if let Motion::Analyzed(report) = &recording.motion {
            for event in report.events.iter().filter(|e| e.kind == EventKind::Motion) {
                lines.push(format!(
                    "• {} から {}　{} の {}",
                    clock(recording.start as f64 + f64::from(event.start)),
                    duration(event.end - event.start),
                    recording.file_name,
                    offset(event.start),
                ));
            }
        }
    }
    let failed = recordings
        .iter()
        .filter(|r| matches!(r.motion, Motion::Failed))
        .count();
    let missing = recordings
        .iter()
        .filter(|r| matches!(r.motion, Motion::Missing))
        .count();
    let mut text = if lines.is_empty() {
        format!("{header}：動きはありませんでした。")
    } else {
        format!("{header}：動き {}件", lines.len())
    };
    let hidden = lines.len().saturating_sub(MAX_LINES);
    for line in lines.iter().take(MAX_LINES) {
        text.push('\n');
        text.push_str(line);
    }
    if hidden > 0 {
        text.push_str(&format!("\nほか {hidden}件"));
    }
    text.push_str(&format!("\n録画 {}本", recordings.len()));
    if failed > 0 || missing > 0 {
        text.push_str(&format!(
            "（解析できなかった {failed}本、解析結果がない {missing}本）"
        ));
    }
    text
}

struct ApiHub {
    http: reqwest::Client,
    base: String,
    bucket: String,
}

impl ApiHub {
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let value: Value = self
                .http
                .post(format!("{}/s3/list_objects_v2", self.base))
                .json(&json!({"bucket": self.bucket, "prefix": prefix, "max_keys": 1000, "continuation_token": token}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for entry in value["contents"].as_array().into_iter().flatten() {
                if let Some(key) = entry["key"].as_str() {
                    keys.push(key.to_owned());
                }
            }
            let next = value["next_continuation_token"].as_str().map(str::to_owned);
            if value["is_truncated"].as_bool() != Some(true) {
                return Ok(keys);
            }
            if next.is_none() || next == token {
                bail!("S3 listing did not advance");
            }
            token = next;
        }
    }

    async fn motion(&self, key: &str) -> Result<Motion> {
        let url = format!("{}/s3/preview/{}/{}", self.base, self.bucket, key);
        let response = self.http.get(url).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Motion::Missing);
        }
        let bytes = response.error_for_status()?.bytes().await?;
        Ok(match serde_json::from_slice::<Sidecar>(&bytes) {
            Ok(Sidecar::Analyzed(report)) => Motion::Analyzed(report),
            Ok(Sidecar::Failed { .. }) => Motion::Failed,
            Err(error) => {
                eprintln!("camera motion report {key} is unreadable: {error}");
                Motion::Failed
            }
        })
    }

    async fn notify(&self, channel: &str, night: &Night, text: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/notifications", self.base))
            .json(&json!({
                "producer": "home-camera",
                "event_id": night.event_id(),
                "channel": channel,
                "text": text,
            }))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "notification was not accepted: HTTP {status}: {}",
                body.chars().take(500).collect::<String>()
            );
        }
        Ok(())
    }
}

/// 晩の録画（mp4）を開始時刻の順に集め、解析結果を読む。
async fn collect(api: &ApiHub, camera: &str, night: &Night) -> Result<Vec<Recording>> {
    let mut keys = Vec::new();
    for prefix in night.prefixes(camera) {
        keys.extend(api.list(&prefix).await?);
    }
    let reports: HashSet<_> = keys
        .iter()
        .filter(|k| k.ends_with(".motion.json"))
        .cloned()
        .collect();
    let mut recordings = Vec::new();
    for key in keys.iter().filter(|k| k.ends_with(".mp4")) {
        let file_name = key.rsplit('/').next().unwrap_or(key).to_owned();
        let Some(start) = file_name
            .split('-')
            .next()
            .and_then(|epoch| epoch.parse::<i64>().ok())
        else {
            continue; // 古い形式（JST の時刻だけの名前）は対象外。
        };
        if !night.contains(start) {
            continue;
        }
        let report_key = format!("{}.motion.json", key.trim_end_matches(".mp4"));
        let motion = if reports.contains(&report_key) {
            api.motion(&report_key).await?
        } else {
            Motion::Missing
        };
        recordings.push(Recording {
            file_name,
            start,
            motion,
        });
    }
    recordings.sort_by(|a, b| (a.start, &a.file_name).cmp(&(b.start, &b.file_name)));
    Ok(recordings)
}

pub async fn run() -> Result<()> {
    let base = env::var("API_HUB_BASE_URL")
        .context("API_HUB_BASE_URL is required")?
        .trim_end_matches('/')
        .to_owned();
    let channel = env::var("SLACK_CHANNEL").context("SLACK_CHANNEL is required")?;
    let bucket = env::var("S3_BUCKET")
        .unwrap_or_else(|_| crate::config::AppConfig::DEFAULT_BUCKET.to_owned());
    let camera = crate::key::sanitize_camera_id(&env::var("CAMERA_ID").unwrap_or_default());
    let api = ApiHub {
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?,
        base,
        bucket,
    };
    let night = Night::ending_before(OffsetDateTime::now_utc());
    let recordings = collect(&api, &camera, &night).await?;
    let text = compose(&night, &recordings);
    println!("{text}");
    api.notify(&channel, &night, &text).await
}

#[cfg(test)]
mod tests;
