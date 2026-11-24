use super::*;
use crate::motion::Event;
use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::{Arc, Mutex};
use time::macros::datetime;

fn night() -> Night {
    Night::ending_before(datetime!(2026-11-24 09:30 +9))
}

fn event(kind: EventKind, start: f32, end: f32) -> Event {
    Event {
        kind,
        start,
        end,
        peak: 0.1,
    }
}

fn analyzed(events: Vec<Event>) -> Motion {
    Motion::Analyzed(Report {
        version: 1,
        analyzed_seconds: 3600.0,
        events,
        scores: None,
        activity: None,
    })
}

/// 2026-11-23 22:00 JST（13:00 UTC）に始まった録画。
const AT_22: i64 = 1_795_438_800;

#[test]
fn night_ends_at_the_latest_9am_jst() {
    assert_eq!(AT_22, datetime!(2026-11-23 22:00 +9).unix_timestamp());
    assert_eq!(night().end, datetime!(2026-11-24 09:00 +9));
    assert_eq!(
        Night::ending_before(datetime!(2026-11-24 08:59 +9)).end,
        datetime!(2026-11-23 09:00 +9)
    );
    assert_eq!(
        Night::ending_before(datetime!(2026-11-24 00:00 UTC)).end,
        datetime!(2026-11-24 09:00 +9)
    );
    assert_eq!(night().event_id(), "motion-summary-2026-11-24");
    // JST の 11/23 09:00〜11/24 09:00 は、UTC では 11/23 00:00〜24:00。
    assert_eq!(night().prefixes("camera"), vec!["camera/2026/11/23/"]);
    assert!(night().contains(AT_22));
    assert!(!night().contains(datetime!(2026-11-24 09:00 +9).unix_timestamp()));
}

#[test]
fn motion_is_listed_with_time_length_and_file_position() {
    let recordings = vec![
        Recording {
            file_name: format!("{AT_22}-s-000000003.mp4"),
            start: AT_22,
            motion: analyzed(vec![
                event(EventKind::Lighting, 100.0, 101.0),
                event(EventKind::Motion, 2659.0, 2814.0),
            ]),
        },
        Recording {
            file_name: format!("{}-s-000000004.mp4", AT_22 + 3600),
            start: AT_22 + 3600,
            motion: analyzed(vec![event(EventKind::Motion, 5.5, 20.0)]),
        },
        Recording {
            file_name: "x.mp4".into(),
            start: AT_22 + 7200,
            motion: Motion::Failed,
        },
        Recording {
            file_name: "y.mp4".into(),
            start: AT_22 + 10800,
            motion: Motion::Missing,
        },
    ];
    assert_eq!(
        compose(&night(), &recordings),
        format!(
            "昨夜のカメラ（11/23 19:00〜11/24 09:00）：動き 2件\n\
             • 22:44:19 から 2分35秒　{AT_22}-s-000000003.mp4 の 44:19\n\
             • 23:00:05 から 15秒　{}-s-000000004.mp4 の 00:05\n\
             録画 4本（解析できなかった 1本、解析結果がない 1本）",
            AT_22 + 3600
        )
    );
}

#[test]
fn quiet_and_empty_nights_are_still_reported() {
    let quiet = vec![Recording {
        file_name: "a.mp4".into(),
        start: AT_22,
        motion: analyzed(vec![event(EventKind::Lighting, 1.0, 2.0)]),
    }];
    assert_eq!(
        compose(&night(), &quiet),
        "昨夜のカメラ（11/23 19:00〜11/24 09:00）：動きはありませんでした。\n録画 1本"
    );
    assert_eq!(
        compose(&night(), &[]),
        "昨夜のカメラ（11/23 19:00〜11/24 09:00）：録画がありません。"
    );
}

#[test]
fn long_lists_are_cut() {
    let events = (0..MAX_LINES + 5)
        .map(|i| event(EventKind::Motion, i as f32 * 60.0, i as f32 * 60.0 + 3.0))
        .collect();
    let text = compose(
        &night(),
        &[Recording {
            file_name: "a.mp4".into(),
            start: AT_22,
            motion: analyzed(events),
        }],
    );
    assert!(text.contains("動き 35件"));
    assert_eq!(
        text.lines().filter(|l| l.starts_with('•')).count(),
        MAX_LINES
    );
    assert!(text.contains("\nほか 5件\n"));
}

#[test]
fn durations_are_short_japanese_text() {
    assert_eq!(duration(0.2), "1秒");
    assert_eq!(duration(120.0), "2分");
    assert_eq!(duration(3725.0), "1時間2分");
}

#[derive(Clone, Default)]
struct Mock {
    objects: Arc<Vec<(String, String)>>,
    notifications: Arc<Mutex<Vec<Value>>>,
}

async fn list(State(mock): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    // 1件ずつページを分けて、ページ送りを確かめる。
    let prefix = body["prefix"].as_str().unwrap();
    let after = body["continuation_token"].as_str().unwrap_or("");
    let keys: Vec<_> = mock
        .objects
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| k.starts_with(prefix) && *k > after)
        .collect();
    Json(json!({
        "contents": keys.first().map(|k| vec![json!({"key": k})]).unwrap_or_default(),
        "is_truncated": keys.len() > 1,
        "next_continuation_token": keys.first(),
    }))
}

async fn preview(State(mock): State<Mock>, UrlPath(path): UrlPath<String>) -> Response {
    let key = path.strip_prefix("home-camera-recordings/").unwrap();
    match mock.objects.iter().find(|(k, _)| k == key) {
        Some((_, body)) => body.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn notify(State(mock): State<Mock>, Json(body): Json<Value>) -> StatusCode {
    mock.notifications.lock().unwrap().push(body);
    StatusCode::ACCEPTED
}

#[tokio::test]
async fn reads_reports_through_api_hub_and_queues_one_notification() {
    let report = json!({"version":1,"analyzed_seconds":3600.0,"events":[{"kind":"motion","start":2659.0,"end":2814.0,"peak":0.28}]});
    let day = "camera/2026/11/23";
    let mock = Mock {
        objects: Arc::new(vec![
            // 前の晩（対象外）
            (format!("{day}/{}-s-0.mp4", AT_22 - 86400), String::new()),
            (format!("{day}/{AT_22}-s-3.motion.json"), report.to_string()),
            (format!("{day}/{AT_22}-s-3.mp4"), String::new()),
            (
                format!("{day}/{}-s-4.motion.json", AT_22 + 3600),
                r#"{"version":1,"error":"ffmpeg failed"}"#.into(),
            ),
            (format!("{day}/{}-s-4.mp4", AT_22 + 3600), String::new()),
            (format!("{day}/{}-s-5.mp4", AT_22 + 7200), String::new()),
        ]),
        ..Mock::default()
    };
    let app = Router::new()
        .route("/s3/list_objects_v2", post(list))
        .route("/s3/preview/{*path}", get(preview))
        .route("/notifications", post(notify))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = ApiHub {
        http: reqwest::Client::new(),
        base: format!("http://{}", listener.local_addr().unwrap()),
        bucket: "home-camera-recordings".into(),
    };
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let recordings = collect(&api, "camera", &night()).await.unwrap();
    assert_eq!(recordings.len(), 3);
    assert!(matches!(recordings[0].motion, Motion::Analyzed(_)));
    assert!(matches!(recordings[1].motion, Motion::Failed));
    assert!(matches!(recordings[2].motion, Motion::Missing));
    let text = compose(&night(), &recordings);
    assert!(text.contains("動き 1件"), "{text}");
    api.notify("C123", &night(), &text).await.unwrap();
    let sent = mock.notifications.lock().unwrap().clone();
    assert_eq!(
        sent,
        vec![json!({
            "producer": "home-camera",
            "event_id": "motion-summary-2026-11-24",
            "channel": "C123",
            "text": text,
        })]
    );
    server.abort();
}
