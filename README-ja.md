# camera-over-tailscale

[English](README.md)

PSRAM 付きの ESP32-S3 単体で Tailnet に参加し、USB カメラの画像を HTTP で 1 枚返すアダプタである。
Tailnet 上の端末から `http://<hostname>/` を開くと JPEG が返る。

```text
Browser ──Tailscale / http://<hostname>/──▶ ESP32-S3 ──USB host──▶ USB カメラ（UVC、MJPEG）
```

Tailscale のノードとして必要な機能（制御プロトコル、WireGuard、DERP、NAT 越え）は Rust で独自に実装している。
初期設定は USB シリアル上の対話シェルで行い、書き込みと設定はブラウザだけで完了する。
[usb-serial-over-tailscale](https://github.com/signal-slot/usb-serial-over-tailscale) のカメラ版であり、Tailscale ノード、セットアップシェル、Web インストーラを共有している。

## 対応状況

実機ではまだ確認していない。
Tailscale ノード、セットアップウィザード、Wi-Fi の扱い、Web インストーラは usb-serial-over-tailscale から変更なしに引き継いでおり、そちらでは N16R8 のボードで確認済みである。
このプロジェクト固有の部分（UVC ホストドライバの組み込み、オンデマンドのストリーム、Tailnet 上の HTTP サーバ）はビルドまでしか通していない。

既知の制約は次のとおりである。

- ESP32-S3 の USB ホストは Full Speed（12 Mbit/s）である。カメラは USB 2.0 の帯域のごく一部しか使えないので、640x480 で毎秒数フレーム程度になり、大きなサイズの交渉を拒むカメラもある。`resolution` と `fps` でカメラが受け付ける値を選ぶ。カメラが出せるものは `camera` で一覧できる。
- MJPEG だけを使う。非圧縮（YUY2）しか出せないカメラには対応していない。ボードには JPEG エンコードに回せる余力がない。
- 同時に処理するリクエストは 1 つである。ブラウザでリロードすると、前のリクエストが終わるまで待つ。
- USB ハブには対応していない。カメラは直接挿す。

### モジュールの型番

flash の容量と PSRAM の方式はビルド時の固定設定なので、CI は ESP32-S3 モジュールの型番ごとにイメージを作る（`firmware/variants/`）。
PSRAM は必須である。フレームバッファ、ネットワーク系スレッドのスタック、TLS のバッファを PSRAM に置いている。

| 型番 | flash | PSRAM | パーティション表 | 状況 |
| --- | --- | --- | --- | --- |
| N16R8 | 16 MB | 8 MB Octal | `partitions.csv`（アプリ 6 MB） | ビルドのみ |
| N8R8 | 8 MB | 8 MB Octal | `partitions.csv` | ビルドのみ |
| N4R8 | 4 MB | 8 MB Octal | `partitions-4m.csv`（アプリ 3 MB） | ビルドのみ |
| N16R2、N8R2、N4R2 | 16、8、4 MB | 2 MB Quad | 上と同じ | ビルドのみ |
| WROOM-2 N16R8V、N32R8V | 16、32 MB Octal | 8 MB Octal | `partitions.csv` | ビルドのみ |
| N4、N8、N16（PSRAM なし） | | なし | | 非対応 |

WROOM-1U（外部アンテナ）は WROOM-1 と同じ型番を使う。MINI-1 の N4R2 は `n4r2` に相当する。

手元で型番を指定してビルドするには、esp-idf-sys に型番の defaults ファイルを渡し、イメージ作成スクリプトにも型番を渡す。

```bash
cd firmware
ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;variants/n8r2.defaults" cargo build --release
tools/mkimage.sh n8r2 release
```

## 構成

| パス | 役割 |
| --- | --- |
| `crates/tsnode` | Tailscale ノード実装。制御プレーン（ts2021 Noise と最小の HTTP/2）、WireGuard、DERP、disco、STUN、smoltcp による TCP 終端。ホストでも ESP-IDF でも動く。 |
| `crates/adapter-core` | 対話式セットアップシェル、スナップショットサーバの HTTP リクエスト解釈と応答の組み立て、UVC コントロールの表（ディスクリプタの解析、パラメータ名、値の符号化）。入出力とデバイス操作を trait で抽象化し、すべてホストで単体テストする。 |
| `tools/hostnode` | tsnode を Linux で動かす検証ツール。実 Tailnet に参加し、テスト画像でスナップショットサーバを公開する。 |
| `firmware` | ESP32-S3 ファームウェア（esp-idf-svc）。設定コンソール、USB ホスト（UVC のストリーミングとコントロール）、Wi-Fi、Tailscale ノード、HTTP サーバ。 |

tsnode の内部は次のモジュールに分かれる。

| モジュール | 内容 |
| --- | --- |
| `controlbase`、`h2`、`control` | `/key` の取得、`POST /ts2021` による Noise IK ハンドシェイク、HTTP/2 での `/machine/register` と `/machine/map`。`/key` は常に TLS、Noise 経路は :80 の平文を試してから TLS に切り替える。 |
| `wireguard` | Noise IKpsk2 ハンドシェイク（initiator と responder）、transport、replay window、rekey と keepalive のタイマ。 |
| `derp` | DERP リレークライアント（TLS、fast-start、ping と pong、NotePreferred）。 |
| `disco`、`stun` | 直接経路のための ping、pong、call-me-maybe と、STUN による公開エンドポイントの検出。 |
| `magicsock` | UDP 直接経路と DERP 経路の選択、ハンドシェイクの処理、ピア表の管理。 |
| `netstack` | smoltcp による Tailnet アドレス上の TCP 終端。 |
| `node` | 上記をスレッドで束ねる。鍵は NVS またはファイルに永続化する。 |

## 使い方

### ボードを準備する（はんだ付けが要る）

PSRAM 付きで USB-C が 2 つ（COM と native USB）ある ESP32-S3 ボードを使う。
裏面の `USB-OTG` パッドをはんだで短絡する（下の写真は短絡前）。

<img src="web/usb-otg-pad.webp" alt="裏面の USB-OTG パッド（短絡前）" width="360">
これは必須で、ボードからカメラへ 5 V を送るためのものである。短絡しないとカメラに電源が入らず、検出されない。
2 つのポートの役割は固定である。**`COM` = 5 V の給電**（充電器か PC）、**`USB` = カメラ**。書き込みと設定のときだけ、`USB` を PC につなぐ。
短絡に伴う注意は後述の「給電と配線」にある。

### ブラウザから書き込む

Chrome か Edge で [signal-slot.github.io/camera-over-tailscale](https://signal-slot.github.io/camera-over-tailscale/) を開くと、ソフトウェアを何も入れずに書き込みと初期設定ができる。
ページは WebSerial でボードの USB Serial/JTAG ポートを開き、ESP Web Tools（esptool-js）で CI がビルドしたバイナリを書く。
同じページの端末でセットアップウィザードも操作できる。
Windows 10 以降、macOS、Linux、ChromeOS で動き、Firefox と Safari とスマートフォンでは動かない。
つなぐのは native 側の USB で、COM 側（CH343）は Windows でドライバが要るので使わない。

ページではモジュールの金属カバーの刻印（`ESP32-S3-WROOM-1-N16R8` など）から型番を選ぶ。
CI（GitHub Actions）は push のたびに全型番をビルドし、`v*` タグでは Release にも `camera-over-tailscale-<型番>.bin`（bootloader、パーティション表、アプリを結合した 1 本）を添付する。
結合したものは `esptool write_flash 0x0` で書けるが、NVS の領域も 0xFF で上書きするので設定と登録が消える。
ページからの更新は 3 つの領域だけを書くので設定は残る。

### 初期設定

未設定の状態で native USB ポートを PC に挿すと、USB シリアル（Espressif USB JTAG/serial、303a:1001）として見える。
ターミナルで開いて Enter を押すと、ウィザードが始まる。

```bash
screen /dev/ttyACM0 115200
```

```text
camera-over-tailscale 0.1.0 - setup mode
Press Enter to start setup, or type a command:
=== Setup ===
Scanning Wi-Fi...
   1) home-wifi        (-48 dBm)
Select network number, or type an SSID (Enter to rescan, q to quit): 1
Password for home-wifi: ********
Connected (192.168.1.42)
Hostname on the tailnet [camera]:
Registering with Tailscale... (press q to stop waiting)
Open this URL in a browser to approve the device:
  https://login.tailscale.com/a/xxxxxxxx
Waiting for approval...
Tailscale is up: camera.example.ts.net 100.x.y.z
Setup complete. Reboot into normal mode now? [Y/n]
```

`setup` は hostname と Tailscale の登録が済んでいれば Wi-Fi の追加だけで終わる。
すべてやり直すには `setup all` を使う。
auth key を使う場合は `authkey tskey-auth-...` を打ってから `login` する。

主なコマンドは次のとおりである。
一覧は `help` で出る。

| コマンド | 内容 |
| --- | --- |
| `wifi <ssid> <pw>` | Wi-Fi を追加して接続する。複数登録でき、起動時と再接続時に一番強い既知の AP を選ぶ。 |
| `wifi list`、`wifi forget <ssid>` | 登録済み Wi-Fi の一覧と削除。 |
| `port <n>` | HTTP サーバの TCP ポート（既定 80）。 |
| `resolution <WxH>`、`resolution auto` | カメラに要求するフレームサイズ。既定の `auto` は 640x480 に一番近い MJPEG のサイズを選ぶ。カメラにないサイズを指定すると一番近いものに切り替える。 |
| `fps <n>`、`fps auto` | 要求するフレームレート。`auto` はそのサイズでのカメラの既定値を使う。カメラが拒んだ値も既定値に切り替える。 |
| `camera` | 接続中のカメラの状態と、出せるサイズとレートの一覧（通常モード、UART シェル）。 |
| `snap` | 1 枚撮影して、サイズと所要時間を表示する（通常モード、UART シェル）。 |
| `status`、`reset`、`reboot` | 状態表示、設定と鍵の消去、再起動。 |

同じシェルは UART ポート（115200 bps、ログと共用）でも常時使える。
Enter を押すとプロンプトが出る。
BOOT を押しながら起動すると、再びセットアップモードになる。
通常モードでは native USB ポートがカメラ用なので、`camera` と `snap` は UART シェルからしか意味を持たない。

### 画像を取る

Tailnet 上の任意の端末から、ブラウザで hostname を開くか、ツールで取得する。

```bash
curl -o snap.jpg http://camera/
```

`/`（`/snapshot.jpg` でも同じ）へのリクエストごとに、新しく撮影した JPEG を `Cache-Control: no-store` 付きで返す。
`HEAD` も受け付ける。後述の `/controls` を除き、他のパスは 404、他のメソッドは 405 を返す。
カメラがない場合は 503、カメラはあるが 8 秒以内にフレームが来ない場合は 504 で、本文に理由を入れる。

### カメラの設定

`/` のクエリパラメータで、撮影前にカメラの UVC コントロールを設定できる。
設定は変更するまで残る（カメラを挿し直した場合も、ボードが再起動するまでは同じ設定を再適用する）。
`/controls` は、カメラが対応しているコントロールを、現在値と範囲付きの JSON で返す。

```bash
curl -o snap.jpg 'http://camera/?wb=auto&zoom=150&focus=80'
curl http://camera/controls
```

| パラメータ | コントロール | 値 |
| --- | --- | --- |
| `exposure` | 露出時間（Camera Terminal） | `auto`、または 100 µs 単位の時間。数値を与えるとカメラを手動露出に切り替える。 |
| `focus` | フォーカス | `auto`、またはカメラの単位での距離。 |
| `zoom`、`iris`、`roll` | ズーム、絞り、ロール | カメラの単位での数値。 |
| `pan`、`tilt` | パンとチルト | 符号付きの数値（UVC の規格では 1/3600 度単位）。 |
| `wb` | ホワイトバランス | `auto`、または色温度（ケルビン）。 |
| `hue` | 色相 | `auto`、または符号付きの数値。 |
| `brightness`、`contrast`、`saturation`、`sharpness`、`gamma`、`gain`、`backlight` | Processing Unit の画質コントロール | カメラの単位での数値。 |
| `powerline` | 電源周波数（フリッカー対策） | `0`、`50`、`60`。 |

値は UVC の生の値なので、範囲はカメラごとに違う。
`/controls` が各コントロールの `min`、`max`、`step`、`def`（カメラの既定値）を示す。
範囲外の値、カメラが対応していないパラメータ、綴りの違うパラメータには 400 とメッセージを返し、カメラが要求を拒んだ場合は 500 を返す。
パラメータは書いた順に適用するので、`wb=auto` の後の `wb=4500` が有効になる。
変更後は次の 2 フレームを捨て、返す画像に新しい設定が反映されるようにしている。

誰も要求していない間、カメラのストリームは止まっている。
最初のリクエストでストリームを始め、露出が落ち着くまで最初の 2 フレームを捨て、次のフレームを返す。
このためカメラによっては 1、2 秒かかる。
最後のリクエストから 10 秒間はストリームを続けるので、続けて要求すれば起動の待ちなしにフレームが返る。

### 給電と配線

給電は COM 側の USB から行う。
PC でも USB 充電器でも構わない。
native 側はカメラをつなぐ USB ホストのポートで、Web カメラはバスパワーなのでここから電流を取る。

native 側から 5 V を出すには、裏面の `USB-OTG` パッドを短絡する。
このパッドは USB-C を 2 つ持つ DevKitC-1 互換ボードの多くにある。ないボードでは、native 側の VBUS への給電を別の方法で確保する必要がある。
これは 2 つの USB-C コネクタの VBUS を直結するので、短絡後は**両方を同時に PC へ挿さない**。
セットアップは native 側だけを PC に、通常運用は COM 側を電源に、native 側をカメラに、と使い分ける。
5 V の電源は 1 A 以上が望ましい。Web カメラは数百 mA を使い、ボードの Wi-Fi のピークがそれに加わる。

## ビルドと書き込み

前提は、`espup` で導入した `esp` ツールチェーン、`ldproxy`、`espflash` である。
ESP-IDF v5.3.3 とコンポーネント `espressif/usb_host_uvc` は、初回ビルド時に `firmware/.embuild/` へ自動で取得される（数 GB、数分）。

```bash
# ホスト側の単体テストと検証ツール
cargo test
cargo run -p hostnode -- probe-control                      # 制御プレーンの疎通（鍵なし登録で AuthURL が返れば良い）
cargo run -p hostnode -- run --hostname tsnode-test        # 実 Tailnet に参加し、テスト画像でスナップショットサーバを公開（承認 URL を表示）

# ファームウェア
cd firmware
cargo build --release
cargo run --release      # espflash flash --monitor --flash-size 16mb --partition-table partitions.csv
```

`~/export-esp.sh` は source しない。
gcc と clang は esp-idf-sys が `.embuild/` に導入したものを使い、espup の xtensa gcc が PATH の先頭にあるとリンクに失敗する。

パーティション表は `firmware/partitions.csv`（factory 6 MB、flash 8 MB と 16 MB 用）と `firmware/partitions-4m.csv`（3 MB、flash 4 MB 用）である。
esptool で書く場合は `tools/mkimage.sh <型番> release` で `dist/<型番>/` にバイナリを作る。

```bash
cd firmware && tools/mkimage.sh n16r8 release
PY=$(ls -d .embuild/espressif/python_env/*/bin/python | head -1)
$PY -m esptool --chip esp32s3 --port /dev/ttyACM0 --baud 921600 write_flash \
  0x0 dist/n16r8/bootloader.bin 0x8000 dist/n16r8/partition-table.bin 0x10000 dist/n16r8/app.bin
```
runner はボードの UART ポートを `/dev/ttyACM0` に固定しているので、環境に合わせて `.cargo/config.toml` を直す。
`hostnode run` は鍵を `hostnode-state.json` に平文で保存する。

## 設計上の判断

- capability version は 106 を名乗り、map 応答は非圧縮で受ける。Headscale は非保証である。
- 認証は承認 URL 方式を既定とし、auth key は任意である。auth key は登録完了後に NVS から削除する。
- Tailscale の ACL（PacketFilter）は受信側で強制する。フィルタが届くまでは全パケットを捨てる。
- DERP の home region は、起動時に各リージョンへ STUN して最小 RTT で選ぶ。STUN が通らなければ `tok`、なければ最小 ID にフォールバックする。
- 直接経路は disco の ping と pong、call-me-maybe で確立する。相手から届いた認証済み UDP パケットの送信元も経路として採用する。
- WireGuard のタイムスタンプは、システム時計と control 時刻との差分、および SNTP で補正する。
- ネットワーク系スレッドのスタックとカメラのフレームバッファは PSRAM に置き、フラッシュを書く制御スレッドだけ内部 RAM に置く。
- カメラは Espressif の `usb_host_uvc` コンポーネントで動かす。MJPEG のフレームをそのまま返すので、ボードでは画像のデコードもエンコードもしない。
- カメラの設定は Camera Terminal と Processing Unit への素の UVC クラス要求である。ユニット ID はコンフィギュレーションディスクリプタから読むが、UVC ドライバがデバイスハンドルを公開しないので、短命の USB ホストクライアントを自前で登録して読む。値は正規化せず UVC の生の値を使うので、`/controls` が示す値がそのままカメラの受け付ける値である。
- ストリームは常時ではなくオンデマンドで動かし、10 秒使われなければ止める。isochronous 転送は Wi-Fi と WireGuard が必要とする CPU を食うし、スナップショットサーバには誰も求めていないフレームの使い道がない。
- フレームバッファは、16 bit/pixel の非圧縮サイズの半分を 2 本持つ。JPEG がそれより大きいカメラではログにフレームバッファのオーバーフローが出るので、解像度を下げる。

## スコープ外

Secure Boot と Flash 暗号化、OTA、Web 管理画面、動画配信（HTTP 上の MJPEG）、複数同時接続、非圧縮フォーマットのカメラ、USB ハブ、専用基板は扱っていない。

## ライセンス

MIT。
Tailscale と WireGuard のプロトコル部分は、プロトコルの仕様と Tailscale（BSD-3-Clause）、wireguard-go（MIT）のソースを元に書いた。
ファームウェアは Espressif の `usb_host_uvc` コンポーネント（Apache-2.0）をリンクする。
