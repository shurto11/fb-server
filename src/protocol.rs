//! fb-server ⇄ クライアント間のメッセージ型（JSON Lines over Unix socket）。
//!
//! サーバーはピクセルを一切扱わない。「今このクライアントは表示してよいか」
//! (visible: bool) だけを通知する。実際の /dev/fb0 への書き込みはクライアント
//! 自身が行う。

use serde::{Deserialize, Serialize};

/// クライアント → サーバー（接続直後に1行だけ送る）。
/// `hello` はクライアント名(レイヤー名)。scenes.toml の `[scenes.X]` の
/// キーと一致すれば、接続している間そのシーンが有効になる。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub hello: String,
}

/// サーバー → クライアント。今表示してよいか。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visible {
    pub visible: bool,
}

/// `fb-server status` 用の予約済みクライアント名。
/// この名前で hello すると通常のレイヤー登録は行わず、現在のシーン名と
/// 接続中クライアント一覧を1行返して切断する。
pub const STATUS_QUERY_NAME: &str = "__status__";

/// `STATUS_QUERY_NAME` へのサーバーからの応答。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StatusReply {
    pub scene: String,
    pub clients: Vec<String>,
}

/// ソケットパス。`$FB_SERVER_SOCK` > `$XDG_RUNTIME_DIR/fb-server.sock` >
/// `/tmp/fb-server.sock`。
pub fn socket_path() -> String {
    if let Ok(p) = std::env::var("FB_SERVER_SOCK") {
        if !p.is_empty() {
            return p;
        }
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => format!("{d}/fb-server.sock"),
        _ => "/tmp/fb-server.sock".to_string(),
    }
}
