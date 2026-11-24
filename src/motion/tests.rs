use super::*;

const W: usize = 16;
const H: usize = 10;

fn params() -> Params {
    Params {
        width: W,
        height: H,
        ..Params::default()
    }
}

/// 背景（明るさ 40）の上に、(x, y) から幅 w・高さ h の明るい四角を置いたフレーム。
fn frame_with_box(x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
    let mut frame = vec![40u8; W * H];
    for row in y..(y + h).min(H) {
        for col in x..(x + w).min(W) {
            frame[row * W + col] = 200;
        }
    }
    frame
}

fn still() -> Vec<u8> {
    vec![40u8; W * H]
}

fn run(frames: &[Vec<u8>], params: Params) -> Vec<Event> {
    let mut detector = Detector::new(params);
    for frame in frames {
        detector.push(frame);
    }
    detector.finish(false).events
}

#[test]
fn still_video_has_no_events() {
    let frames = vec![still(); 120];
    assert!(run(&frames, params()).is_empty());
}

#[test]
fn small_noise_below_pixel_delta_is_ignored() {
    let frames: Vec<_> = (0..120)
        .map(|i| {
            still()
                .into_iter()
                .enumerate()
                .map(|(p, v)| v + ((p + i) % 7) as u8)
                .collect()
        })
        .collect();
    assert!(run(&frames, params()).is_empty());
}

#[test]
fn sustained_motion_becomes_one_event() {
    // 10秒静止 → 5秒間、四角が右へ動く → 10秒静止（2fps）
    let mut frames = vec![still(); 20];
    frames.extend((0..10).map(|i| frame_with_box(i, 3, 3, 3)));
    frames.extend(vec![still(); 20]);
    let events = run(&frames, params());
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event.kind, EventKind::Motion);
    assert!(event.start >= 9.5 && event.start <= 10.5, "{event:?}");
    assert!(event.end >= 14.5, "{event:?}");
    assert!(event.peak > 0.05);
}

#[test]
fn single_frame_spikes_are_not_events_even_when_close_together() {
    // 4秒ごとに1フレームだけ変化する（つなげる間隔の10秒より短い）。
    let frames: Vec<_> = (0..120)
        .map(|i| {
            if i % 8 == 4 {
                frame_with_box(2, 2, 6, 6)
            } else {
                still()
            }
        })
        .collect();
    assert!(run(&frames, params()).is_empty());
}

#[test]
fn short_motion_below_min_seconds_is_dropped() {
    let mut frames = vec![still(); 20];
    frames.extend((0..4).map(|i| frame_with_box(i * 2, 3, 3, 3))); // 2秒
    frames.extend(vec![still(); 20]);
    assert!(run(&frames, params()).is_empty());
}

#[test]
fn nearby_motion_is_merged() {
    let mut frames = vec![still(); 10];
    frames.extend((0..8).map(|i| frame_with_box(i, 3, 3, 3)));
    frames.extend(vec![still(); 6]); // 3秒の途切れ（つなげる間隔より短い）
    frames.extend((0..8).map(|i| frame_with_box(i, 3, 3, 3)));
    frames.extend(vec![still(); 30]);
    let events = run(&frames, params());
    assert_eq!(events.len(), 1, "{events:?}");
}

#[test]
fn whole_frame_change_is_short_lighting_event() {
    // 明るさ 40 → 180 に切り替わり、そのまま続く。
    let mut frames = vec![still(); 20];
    frames.extend(vec![vec![180u8; W * H]; 40]);
    let events = run(&frames, params());
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, EventKind::Lighting);
    assert!(events[0].end - events[0].start <= 1.5, "{events:?}");
}

#[test]
fn masked_region_is_ignored() {
    // 右上の 1/4 で数字が変わり続ける（時計表示の想定）。
    let frames: Vec<_> = (0..120)
        .map(|i| {
            if i % 2 == 0 {
                frame_with_box(12, 0, 4, 2)
            } else {
                still()
            }
        })
        .collect();
    let mut masked = params();
    masked.masks = vec![[0.75, 0.0, 0.25, 0.25]];
    assert!(run(&frames, masked).is_empty());
}

#[test]
fn activity_counts_changes_per_pixel() {
    let frames = vec![still(), frame_with_box(0, 0, 1, 1), still()];
    let mut detector = Detector::new(params());
    for frame in &frames {
        detector.push(frame);
    }
    let report = detector.finish(true);
    let activity = report.activity.unwrap();
    assert_eq!(activity[0], 2);
    assert_eq!(activity[1], 0);
    assert_eq!(report.scores.unwrap().len(), 3);
}

#[test]
fn masks_are_parsed_and_validated() {
    assert_eq!(
        parse_masks("0.8,0,0.2,0.1; 0,0.9,1,0.1").unwrap(),
        vec![[0.8, 0.0, 0.2, 0.1], [0.0, 0.9, 1.0, 0.1]]
    );
    assert!(parse_masks("").unwrap().is_empty());
    assert!(parse_masks("0.5,0.5,0.5").is_err());
    assert!(parse_masks("1.5,0,0.1,0.1").is_err());
    assert!(parse_masks("0,0,0,0.1").is_err());
}

/// 実際の FFmpeg で、黒い背景の上を白い四角が5秒間動く映像を作って解析する。
#[test]
#[ignore = "requires FFMPEG_BIN"]
fn ffmpeg_synthetic_video_with_moving_box() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.mp4");
    let ffmpeg = env::var("FFMPEG_BIN").expect("FFMPEG_BIN required");
    let status = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=0x202020:s=640x360:r=15:d=40",
            "-f",
            "lavfi",
            "-i",
            "color=c=white:s=120x120:r=15:d=40",
            // drawbox の x では時刻 t が使えないので、overlay で四角を動かす。
            "-filter_complex",
            "[0][1]overlay=x='40+(t-15)*60':y=120:enable='between(t,15,20)'",
            "-c:v",
            "libx264",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let report = analyze_file(&path, &Params::default(), false).unwrap();
    assert!((report.analyzed_seconds - 40.0).abs() < 1.0, "{report:?}");
    assert_eq!(report.events.len(), 1, "{report:?}");
    let event = &report.events[0];
    assert_eq!(event.kind, EventKind::Motion);
    assert!(event.start >= 14.5 && event.start <= 15.5, "{event:?}");
    assert!(event.end >= 19.5 && event.end <= 21.0, "{event:?}");
}
