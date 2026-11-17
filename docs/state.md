# 録画ファイルの扱い

`--record` が spool（PVC）に連続したファイルを確定し、`--upload` が別のプロセスで S3 へ送ります。
2 つのプロセスは、spool のファイルロックで、それぞれ 1 つしか動かないようにしています。

## spool の中身

| ファイル | 意味 |
|---|---|
| 完成した `.mp4` と `.json`（manifest） | 未送信の正本 |
| `.part` | 録画中 |
| `.corrupt` | 壊れている。調べるために残す |
| `.quarantine/<incident>/` | 調べ終わって隔離したもの |

- ファイル名は `segment-<session 開始の epoch>-<9 桁の連番>.part` です。同じ秒に複数できても上書きしません。
- 開始時刻は、session の開始時刻（UTC）と、FFmpeg が出す CSV のメディア時刻から求めます。S3 のキーにも連番を含めます。
- CSV は、改行まで書かれた行だけを「完成」の通知として扱います。重複した名前、不正なパスや時刻は拒否します。
- epoch だけの古い形式の名前と、その manifest も、回収して送信できます。
- 再試行の情報（attempts、next_attempt_at）は manifest に atomic rename で保存します。

## 録画

- 録画するのは JST 19:00 から翌 09:00 です。
- RTSP の接続は 30 秒でタイムアウトします。完成したファイルが「1 ファイルの長さの 2 倍 + 60 秒」出てこないと、FFmpeg をつなぎ直します。
- spool が上限に達した、または空きが足りないときは、未送信のファイルを消さずに録画を止めます。
- FFmpeg のエラーと再接続の理由は recorder のログに出します。RTSP の URL とパスワードは伏せます。

## 送信

1. S3 の固定のキーを HEAD する。404 のときだけ PUT に進む。
2. ローカルの checksum と、S3 のキー・サイズを照合する。
3. 照合できたら、動画 → manifest の順に spool から消す。

- S3 側で確かめるのはサイズです。S3 上の内容の checksum までは確かめません。
- 応答を受け取れなかった、消している途中で止まった、という場合は、同じキーの HEAD からやり直せます。
- ローカルと S3 の両方にない、サイズが合わない、認証に失敗した、という場合は、消さずに残します。
- JSON が壊れている、日時や必須の項目がおかしい manifest は、manifest ごとに隔離し、動画も残します。キーを推測して作り直すことはしません。
  ほかの正常なファイルの送信は続けます。

## 壊れたファイルを隔離する

録画中に Pod やホストが止まると、書きかけのファイル（moov atom のない mp4）が残り、`CameraPartialSegment` が出ます。
調べたうえで、次の手順で隔離します。

1. 原本、CSV、checksum、調べた理由を、別の場所にバックアップする。
2. その session のファイルを `/spool/.quarantine/<incident>/` へ移す。

`.quarantine` の中は、自動の回収、送信、未確認ファイルの数、送信待ちの容量の対象外で、消されることもありません。
ただし PVC 全体の空き容量には数えられます。原因を調べる前の隔離や、一括の掃除には使いません。

## 監視

recorder が 9090 番の `/metrics` で出します。

| 指標 | 意味 |
|---|---|
| `home_camera_recording_active` | 録画中か |
| `home_camera_recording_stalled` | 録画する時間帯なのに、ファイルが確定していない |
| `home_camera_invalid_manifests` | 壊れた manifest の数 |
| `home_camera_spool_stats_success` | spool の集計に成功したか。失敗の間、ほかの spool の値は古い可能性がある |

- `recording_stalled` の猶予は再接続と同じ長さですが、つなぎ直してもリセットしません。ファイルが正常に確定したときに解除します。
- 時間帯の外と、容量で止まっている間は、監視の猶予を解除し、録画を再開したところから測り直します。
- プロセスが生きていることだけでは、録画できているとは見なしません。

アラートの条件と発火時の対応は [home-lab-k8s の監視](https://github.com/home-lab-ksera/home-lab-k8s/blob/main/docs/observability.md#アプリのキュー) にあります。
