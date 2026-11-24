# home-camera

カメラの RTSP を連続して録画し、S3 へ送る Rust 製のアプリです。JST 19:00 から翌 09:00 に録画します。DB は使いません。

- 録画ファイルの扱い、回復、監視: [docs/state.md](docs/state.md)
- 本番の配置: [home-lab-k8s のアプリ一覧](https://github.com/home-lab-ksera/home-lab-k8s/blob/main/docs/apps.md#home-camera)

## 実行モード

同じ spool を mount した 2 つのプロセスで動かします。引数なしや、それ以外の引数はエラーです。

| 引数 | 内容 |
|---|---|
| `--record` | `recorder.lock` を取り、RTSP を録画して、完成した mp4 と manifest を spool に確定する |
| `--upload` | `uploader.lock` を取り、完成したファイルを api-hub 経由で S3 に送り、動きのあった区間を解析して結果も送り、spool から消す |
| `--analyze [--debug] FILE.mp4` | 調整用。録画 1 本を解析して、結果の JSON を標準出力に出す。`--debug` は各フレームの値も出す |

壊れたファイルや manifest は消さずに残します。容量が足りないときは、未送信のファイルを消さずに録画を止めます。

## 環境変数

| 変数 | 既定 | 内容 |
|---|---|---|
| `RTSP_URL` | （recorder で必須） | カメラの URL。uploader には要らない |
| `API_HUB_BASE_URL` | （必須） | api-hub の URL。S3 の認証情報は api-hub が持つ |
| `SPOOL_DIR` | `/spool` | spool の場所 |
| `SEGMENT_SECONDS` | `3600` | 1 ファイルの長さ（10〜3600 秒） |
| `SPOOL_MAX_BYTES` | 80GiB | spool の上限 |
| `S3_BUCKET` | `home-camera-recordings` | 保存先の bucket |
| `CAMERA_ID` | `camera` | S3 のキーの先頭 |
| `FFMPEG_VIDEO_CODEC`、`FFMPEG_AUDIO_CODEC` | `copy` | codec |
| `FFMPEG_AUDIO_BITRATE` | `128k` | 音声を再エンコードするときの bitrate |
| `FFMPEG_PRESET`、`FFMPEG_CRF` | `veryfast`、`23` | libx264 を使うときの設定（CRF は 0〜51） |
| `MOTION_ANALYSIS` | （有効） | `off` で、uploader の動きの解析を止める |
| `MOTION_THRESHOLD` | `0.03` | 画面のこの割合以上が1秒前から変わったフレームを「動きあり」とする |
| `MOTION_PIXEL_DELTA` | `20` | 明るさがこれを超えて変わった画素を「変わった」とする（0〜255） |
| `MOTION_MIN_SECONDS` | `3` | 「動きあり」がこの秒数以上続いたものだけを区間にする |
| `MOTION_MERGE_GAP_SECONDS` | `60` | この秒数未満の途切れは前後の区間をつなげる |
| `MOTION_MASKS` | （なし） | 解析から外す領域。`x,y,w,h;...`（画面に対する 0〜1 の割合） |

本番の StatefulSet、PVC、Secret の参照は、[home-lab-k8s の manifests/apps/home-camera/](https://github.com/home-lab-ksera/home-lab-k8s/tree/main/manifests/apps/home-camera) が正本です。

## テスト

```bash
cargo test --locked
FFMPEG_BIN=/path/to/ffmpeg FFPROBE_BIN=/path/to/ffprobe cargo test --locked --lib -- --ignored
```

2 行目は回復と動きの解析の試験で、FFmpeg が要ります。人工の映像と、模擬の HTTP / RTSP を使います。

## 配布

`build-and-push.yaml` が、`state-tests.yml`（回復の試験）と home-lab-k8s の共通 workflow（fmt・clippy・cargo-audit）を実行し、
両方が成功したら、既定ブランチへの push のときだけイメージを Harbor へ公開します。
公開からクラスタへの反映までは [アプリ配布](https://github.com/home-lab-ksera/home-lab-k8s/blob/main/docs/app-delivery.md) を参照してください。
更新のときは Pod が入れ替わるため、録画中だと書きかけのファイルが残ることがあります（[書きかけのファイル](docs/state.md#壊れたファイルを隔離する)）。
