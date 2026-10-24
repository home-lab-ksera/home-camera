# 常駐録画と再送

`--record` はDBに依存せず録画し、`--upload` が完成segmentをcamera DBへ登録・送信する。`--migrate` はDB migrationのみ。
引数なしは切替前の1回録画の動作を残す。

JST 19:00〜翌09:00に、1つのFFmpeg processでsegmentを連続生成する。
SPOOL_DIRは既定/spool、SEGMENT_SECONDSは300（10〜900）、SPOOL_MAX_BYTESは80GiB。
codec copyではkeyframe間隔により長さが変わる。CSVでclose済みと確認したsegmentをffprobeで確認し、実duration・サイズ・SHA-256とmanifestを確定する。
segmentファイルを開いたUTC秒を開始、実durationから終了を記録する。

spoolの上限または空き2GiB未満で録画を止め、未送信を自動削除しない。現在書き込み中のpartと完成mp4を区別し、起動時に回復する。
回復できないpartはcorruptとして保持・監視する。spool上のfile lockでwriterを1つにする。
PVC自体が利用不能な障害では録画できない。

uploaderはRTSP資格情報を持たず、app/verify-fullでDBへ接続する。pool上限2、migration1。
各segmentの保存先は固定で、PUT後DB確定前に停止してもHEADで同じkeyとサイズを照合して回復する。claim一致で保存完了を確定した後だけspoolを削除する。
SHA-256はspoolの不変性確認に使う。S3側のサイズ確認をremote checksum検証と同一視しない。
DB/S3障害中はファイルを保持して再試行する。

recorderの9090にhealth/metricsを提供し、spool bytes、最古未送信の経過時間、容量停止、録画中、不完全segment、FFmpeg再起動回数を公開する。
RTSP URLはログへ出さず、Kubernetes Secretを参照する。

```bash
cargo test --locked
FFMPEG_BIN=/usr/bin/ffmpeg FFPROBE_BIN=/usr/bin/ffprobe \
TEST_DATABASE_URL=postgres://.../isolated_test \
cargo test --locked --lib persistent::tests -- --ignored
```

テスト映像は人工的な青一色のMP4。連続segment生成、DB/S3停止、PUT後DB確定前停止、不完全segment回復を隔離環境で検証する。
[旧CronJobからの切替](../../k8s_myHome/docs/application-postgresql-cutover.md)は旧writerの停止確認後に行う。
