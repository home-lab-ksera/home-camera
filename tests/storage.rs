use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use home_camera::{config::AppConfig, storage::upload_file};

async fn upload_with_status(status: u16) -> Result<(), home_camera::error::AppError> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let payload = vec![0xab; 9 * 1024 * 1024 + 17];
    let expected = payload.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        let header_end = loop {
            let n = stream.read(&mut buffer).unwrap();
            assert!(n > 0);
            received.extend_from_slice(&buffer[..n]);
            if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(received[..header_end].to_vec()).unwrap();
        let target = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(url.path(), "/s3/put_object_stream");
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["bucket"], "home-camera-recordings");
        assert_eq!(query["key"], "camera/日本語 +&#/12.mp4");
        assert!(headers.contains("content-type: video/mp4\r\n"));
        assert!(headers.contains(&format!("content-length: {}\r\n", expected.len())));
        assert!(!headers.to_lowercase().contains("authorization:"));
        while received.len() - header_end < expected.len() {
            let n = stream.read(&mut buffer).unwrap();
            assert!(n > 0);
            received.extend_from_slice(&buffer[..n]);
        }
        assert_eq!(&received[header_end..], expected);
        write!(
            stream,
            "HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        )
        .unwrap();
    });
    let config = AppConfig::from_map(&HashMap::from([
        ("RTSP_URL".into(), "rtsp://camera/live".into()),
        ("API_HUB_BASE_URL".into(), format!("http://{address}/")),
    ]))
    .unwrap();
    let path = std::env::temp_dir().join(format!(
        "home-camera-upload-{}-{status}.mp4",
        std::process::id()
    ));
    std::fs::write(&path, payload).unwrap();
    let result = upload_file(&config, "camera/日本語 +&#/12.mp4", &path).await;
    std::fs::remove_file(path).unwrap();
    server.join().unwrap();
    result
}

#[tokio::test]
async fn streams_recording_to_api_hub() {
    upload_with_status(200).await.unwrap();
}

#[tokio::test]
async fn rejects_api_hub_failure_and_redirect() {
    for status in [500, 307] {
        let error = upload_with_status(status).await.unwrap_err();
        assert!(error.to_string().contains(&status.to_string()));
    }
}
