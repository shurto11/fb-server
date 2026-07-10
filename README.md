# fb-server

複数のプログラムがそれぞれ `/dev/fb0` に直接書き込むと、互いの描画を上書きし合って
画面がチカチカする問題がある(fbterm の再描画、touch-claude の常駐アイコン、
task-var のタスクバー、fbhalf/touch-paint/tmux-session の全画面表示などが競合する)。

fb-server は、この「今どのクライアントが表示してよいか(visible)」を集中管理する
デーモン。[touch-server](../touch/touch-server) が「タッチ入力の唯一の読み手」と
して振り分けるのと対になる: fb-server は「フレームバッファ書き込みの許可」を
一箇所に集約する。

## 仕組み

```
fb-server serve (デーモン)
  1. scenes.toml を読み込む(シーン名 → 許可レイヤー名一覧)
  2. Unix domain socket で待受。クライアントは接続直後に {"hello":"<名前>"} を送る
  3. hello の名前が scenes.toml の [scenes.X] と一致すれば、そのシーンが有効になる
     (最後に接続したものを優先。切断で自動的に前の状態へ戻る。誰も居なければ default)
  4. 現在のシーンの許可レイヤー一覧を見て、接続中の全クライアントへ
     {"visible": bool} を配信する
  5. "status-bar" レイヤーの有無に応じて `tmux set -g status on/off` を実行する
     (tmux ステータスバー自体はソケットクライアントではないため特別扱い)

クライアント
  - 接続時に {"hello":"<自分の名前>"} を送る
  - {"visible":true/false} を受け取り、false の間は /dev/fb0 への書き込みを止める
  - true → false に変わった瞬間、自分の描画領域を一度クリアする
  - ピクセルの実際の書き込みは今まで通り各クライアント自身が行う
    (fb-server はピクセルを一切扱わない)
```

## プロトコル (JSON Lines over Unix socket)

- ソケット: `$XDG_RUNTIME_DIR/fb-server.sock`(無ければ `/tmp/fb-server.sock`)。
  `FB_SERVER_SOCK` で上書き可。
- クライアント→サーバー(接続直後・1行): `{"hello":"task-var"}`
- サーバー→クライアント: `{"visible":true}` / `{"visible":false}`
  (接続直後に1回、以降シーンが変わるたびに配信)

## シーン定義 (scenes.toml)

`$FB_SERVER_CONFIG` > `$XDG_CONFIG_HOME/fb-server/scenes.toml` >
`~/.config/fb-server/scenes.toml` の順に探す。まずリポジトリ同梱の
[`scenes.toml`](./scenes.toml) をコピーする:

```sh
mkdir -p ~/.config/fb-server
cp scenes.toml ~/.config/fb-server/scenes.toml
```

フォーマット:

```toml
default = ["status-bar", "task-var", "touch-claude", "spotatui-pip"]

[scenes.ssbrowse]
extends = "default"
add = ["ssbrowse"]

[scenes.touch-paint]
layers = ["status-bar", "task-var", "touch-paint"]
```

- `extends + add`: 指定シーン(既定 `"default"`)のレイヤー一覧 + 追加分
- `layers`: レイヤー一覧を完全に指定する(継承しない)

## 使い方

```bash
# デーモン起動
cargo run --release -- serve

# 別ターミナルから現在の状態を確認
cargo run --release -- status
```

## 環境変数

| 変数 | 既定値 | 説明 |
|------|--------|------|
| `FB_SERVER_SOCK` | `$XDG_RUNTIME_DIR/fb-server.sock` | ソケットパス |
| `FB_SERVER_CONFIG` | `~/.config/fb-server/scenes.toml` | シーン定義ファイルのパス |

## ファイル構成

- `src/main.rs` — サブコマンド分岐 (`serve` / `status`)
- `src/protocol.rs` — メッセージ型 (`Hello` / `Visible` / `StatusReply`)
- `src/scenes.rs` — `scenes.toml` の読み込みとシーン解決
- `src/server.rs` — 接続管理・シーン判定・`visible` 配信・ステータスバー切替
- `src/tmux.rs` — tmux コマンド実行の共通ヘルパ([touch-server](../touch/touch-server)
  と共通)
- `scenes.toml` — シーン定義のサンプル(このリポジトリの `design.md` の表を反映)

## クライアント側の実装

各プログラムは `src/fb_client.rs` を自分のプロジェクトにコピーして使う
([touch-server](../touch/touch-server) の `touch_client.rs` と同じ慣習)。
リファレンス実装は [`touch/task-var`](../touch/task-var) を参照。

## 既知の制約

mpv の DRM/TTY 直接描画(dopagaki の動画再生)や外部ディスプレイの DRM 直接出力
(fbhalf/ssbrowse の `--display` モード)は、fb-server 経由でピクセルを
やり取りしているわけではない。fb-server はこれらの描画開始/終了の
タイミングで排他制御(visible の on/off)を行うだけで、実際の出力経路までは
関与しない。
