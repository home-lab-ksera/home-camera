# home-camera

Batch app for hourly RTSP recording and upload to RustFS (S3-compatible storage).

## What it does

- Connects to an RTSP camera URL from `RTSP_URL`
- Records stream for `RECORD_SECONDS` (default: 3600) using ffmpeg
- Creates MP4 file locally
- Streams MP4 to api-hub (`POST /s3/put_object_stream?bucket=...&key=...`), which uploads to RustFS
- Uses object key format: `camera/YYYY/MM/DD/HH.mp4` (JST)

## Required environment variables

- `RTSP_URL`
- `API_HUB_BASE_URL` (e.g. `http://api-hub.sandbox.svc:3000`)

S3 credentials and endpoint configuration belong to api-hub; home-camera does not need them.
Uploads are streamed from disk so recording size does not determine memory usage.

## Optional environment variables

- `S3_BUCKET` (default: `home-camera-recordings`)
- `CAMERA_ID` (default: `camera`)
- `RECORD_SECONDS` (default: `3600`)
- `FFMPEG_VIDEO_CODEC` (default: `copy`)
- `FFMPEG_AUDIO_CODEC` (default: `copy`)
- `FFMPEG_AUDIO_BITRATE` (default: `128k`)
- `FFMPEG_LOGLEVEL` (default: `warning`)
- `FFMPEG_PRESET` (default: `veryfast`)
- `FFMPEG_CRF` (default: `23`, range: `0..=51`)

Notes:

- Default pipeline favors stability and quality (`copy` video + `copy` audio)
- `FFMPEG_PRESET` and `FFMPEG_CRF` are only used when `FFMPEG_VIDEO_CODEC=libx264`

## Local run

```bash
cargo run --release
```

## Tests (PBT-first)

```bash
cargo test
```

Property-based tests cover:

- S3 key generation shape and camera id sanitization
- Config parsing constraints
- Retry backoff monotonicity and upper cap

## Kubernetes

- CronJob manifest: `k8s/cronjob.yaml`
- Optional ConfigMap example: `k8s/configmap-ref.yaml` (the CronJob sets `API_HUB_BASE_URL` directly)
- Secret template: `k8s/secret.example.yaml`

Production manifests are managed in `k8s_myHome/manifests/apps/home-camera/`.
Deploy an api-hub version supporting `/s3/put_object_stream` before rolling out home-camera.

For a standalone example, apply in order:

```bash
kubectl apply -f k8s/configmap-ref.yaml
kubectl apply -f k8s/secret.example.yaml
kubectl apply -f k8s/cronjob.yaml
```
