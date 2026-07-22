//! fb-server ⇄ クライアント間のメッセージ型（JSON Lines over Unix socket）。
//!
//! サーバーはピクセルを一切扱わない。「今このクライアントは表示してよいか」
//! (visible: bool) だけを通知する。実際の /dev/fb0 への書き込みはクライアント
//! 自身が行う。

use serde::{Deserialize, Serialize};

/// クライアント → サーバー（接続直後に1行だけ送る）。
/// `hello` はクライアント名(レイヤー名)。scenes.toml の `[scenes.X]` の
/// キーと一致すれば、接続している間そのシーンが有効になる。
///
/// `session` は任意の tmux セッションID(`$0` 形式)。指定すると、その
/// セッションがアクティブ(いずれかの tmux クライアントが表示中)な間だけ
/// visible=true になる(シーンによる許可との AND)。
///
/// `rect` は任意の描画領域(フレームバッファ上のピクセル座標)。申告すると
/// 重なり調停の対象になる: シーンのレイヤー一覧で自分より前(= 優先度が高い)
/// の表示中クライアントと矩形が重なると、その重なり領域が `clip`(描画禁止
/// 矩形)として通知される。クライアントはその矩形を避けて描画すればチカチカ
/// しない。未申告のクライアントは調停に参加しない(隠しも隠されもしない)。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub hello: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
}

/// フレームバッファ上の矩形(ピクセル座標、左上原点)。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// 2つの矩形の重なり部分。重ならなければ None(幅/高さ 0 や辺が接する
    /// だけの場合も含む)。
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.w).min(other.x + other.w);
        let y1 = (self.y + self.h).min(other.y + other.h);
        (x1 > x0 && y1 > y0).then(|| Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

/// サーバー → クライアント。今表示してよいか。
/// `reason` は visible=false の理由。`"session"` はシーン上は許可されている
/// がセッション不一致で隠された場合(クライアントはクリア後にターミナルの
/// 再描画を要求してよい)。それ以外(シーンによる非許可)では省略される。
///
/// `clip` は描画禁止矩形のリスト(フレームバッファ絶対座標)。自分より優先度が
/// 高い表示中クライアントが占める、自分の rect と重なる領域。visible=true でも
/// この矩形の内側は描いてはならない(上位レイヤーが描く領域なので、描くと
/// 交互上書きでチカチカする)。空なら全面を描いてよい。rect を申告していない
/// クライアントには常に空。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Visible {
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clip: Vec<Rect>,
    /// 現在アクティブなシーン名。クライアントが「どのシーンか」で挙動を変える
    /// ために使う(例: task-var は fbhalf シーンの間だけスワイプ表示モードに
    /// なる)。判定に使わないクライアントは無視してよい。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
}

/// クライアント → サーバー(hello の後、描画領域が変わるたびに1行送ってよい)。
/// 現在の描画領域を申告し直す。`rect` を省略/null にすると領域申告を取り消す
/// (以降は調停対象外)。auto 追従で描画領域が動くクライアント(fbhalf など)が
/// 使う。hello 後にこれ以外の入力を送らないクライアントは影響を受けない。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RectUpdate {
    #[serde(default)]
    pub rect: Option<Rect>,
}

/// `fb-server status` 用の予約済みクライアント名。
/// この名前で hello すると通常のレイヤー登録は行わず、現在のシーン名と
/// 接続中クライアント一覧を1行返して切断する。
pub const STATUS_QUERY_NAME: &str = "__status__";

/// `STATUS_QUERY_NAME` へのサーバーからの応答。
/// `clients` の要素はセッション拘束クライアントなら "name (session $N)"。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StatusReply {
    pub scene: String,
    pub clients: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session: Option<String>,
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
