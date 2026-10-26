# home-camera

JST 19:00〜翌09:00にRTSPを連続segment録画し、永続spoolからS3へ送る。DB接続は不要。

- `home-camera --record`: recorder.lockを取得し、完成mp4/manifestをPVCへ確定する。
- `home-camera --upload`: uploader.lockを取得し、固定キーのHEAD/PUT確認後にspoolを解放する。

同じspoolをmountする別プロセスで実行する。引数なし・旧migration・誤った引数は失敗する。破損segment/manifestは保持し、容量不足時は未送信を消さず録画を停止する。[回復と監視](docs/state.md)を参照。

必須設定はrecorderの`RTSP_URL`と`API_HUB_BASE_URL`。uploaderにRTSP資格情報は不要。S3資格情報はapi-hubが持つ。`SPOOL_DIR`は既定`/spool`、`SEGMENT_SECONDS`は300秒（10〜900）、`SPOOL_MAX_BYTES`は80GiB。

`S3_BUCKET`はhome-camera-recordings、`CAMERA_ID`はcamera。codecは`FFMPEG_VIDEO_CODEC`/`FFMPEG_AUDIO_CODEC`が既定copy。audio bitrateは128k。libx264使用時は`FFMPEG_PRESET`（veryfast）と`FFMPEG_CRF`（23、0〜51）を使う。

本番のStatefulSet/PVC/Secret参照は`k8s_myHome/manifests/apps/home-camera/`が正本。旧CronJobのstandalone exampleは廃止した。

```bash
cargo test --locked
FFMPEG_BIN=/path/to/ffmpeg FFPROBE_BIN=/path/to/ffprobe cargo test --locked --lib persistent::tests -- --ignored
```

回復試験は人工映像と模擬HTTP/RTSPを使い、DB設定なしで実行する。
