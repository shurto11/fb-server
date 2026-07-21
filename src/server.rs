//! デーモン本体。クライアントの接続/切断だけを追跡し、現在のシーンに応じた
//! `visible` を各クライアントへ配信する。ピクセルは一切扱わない。
//!
//! hello で tmux セッションID (`session`) を申告したクライアントは、その
//! セッションがアクティブな間だけ visible=true になる(シーン許可との AND)。
//! アクティブセッションはセッション拘束クライアントが居る間だけ tmux を
//! ポーリングして追跡する。
//!
//! hello で描画領域 (`rect`) を申告したクライアント同士は重なりを調停する:
//! シーンのレイヤー一覧の並びが優先度(先頭が最上位)で、優先度の高い表示中
//! クライアントと矩形が重なる下位クライアントには、その重なり領域が
//! `clip`(描画禁止矩形)として配られる。下位はその矩形を避けて描くので、
//! 領域全体を隠さずに交互上書き(チカチカ)だけを防げる。

use crate::protocol::{socket_path, Hello, Rect, StatusReply, Visible, STATUS_QUERY_NAME};
use crate::scenes::Config;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct Client {
    id: u64,
    name: String,
    /// hello で申告された tmux セッションID(`$0` 形式)。None なら無条件。
    session: Option<String>,
    /// hello で申告された描画領域。None なら重なり調停に参加しない。
    rect: Option<Rect>,
    write: UnixStream,
}

struct Shared {
    clients: Mutex<Vec<Client>>,
    /// tmux クライアントが今表示しているセッションID。取得できない間は None
    /// (None の間はセッション拘束を適用しない = フェイルオープン)。
    active_session: Mutex<Option<String>>,
}

pub fn serve() -> Result<()> {
    let config_path = Config::default_path()?;
    let config =
        Arc::new(Config::load(&config_path).with_context(|| {
            format!("scenes.toml を読み込めません: {}", config_path.display())
        })?);

    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!("[fb-server] listening on {path} (config: {})", config_path.display());

    let shared = Arc::new(Shared {
        clients: Mutex::new(Vec::new()),
        active_session: Mutex::new(None),
    });
    let ids = AtomicU64::new(1);

    {
        let shared = shared.clone();
        let config = config.clone();
        thread::spawn(move || watch_active_session(shared, config));
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let id = ids.fetch_add(1, Ordering::Relaxed);
        let shared = shared.clone();
        let config = config.clone();
        thread::spawn(move || handle_client(id, stream, shared, config));
    }
    Ok(())
}

/// tmux クライアントが今表示しているセッションIDを返す。複数クライアントが
/// 居る場合は最後に操作されたもの(client_activity 最大)を採用する。
fn query_active_session() -> Option<String> {
    let out = crate::tmux::capture(&["list-clients", "-F", "#{client_activity} #{session_id}"]);
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let activity: u64 = it.next()?.parse().ok()?;
            Some((activity, it.next()?.to_string()))
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, sid)| sid)
}

/// セッション拘束クライアントが接続している間、アクティブセッションを
/// ポーリングし、切り替わったら visible を再配信する。
fn watch_active_session(shared: Arc<Shared>, config: Arc<Config>) {
    loop {
        thread::sleep(Duration::from_millis(500));
        let bound = shared.clients.lock().unwrap().iter().any(|c| c.session.is_some());
        if !bound {
            // 誰も拘束されていない間は tmux を叩かない。次の拘束クライアント
            // 接続時に古い値で判定しないよう None に戻しておく。
            *shared.active_session.lock().unwrap() = None;
            continue;
        }
        let now = query_active_session();
        let changed = {
            let mut cur = shared.active_session.lock().unwrap();
            if *cur != now {
                *cur = now;
                true
            } else {
                false
            }
        };
        if changed {
            recompute_and_broadcast(&shared, &config);
        }
    }
}

/// 現在のシーン名を決める: レイヤー名が scenes.toml の `[scenes.X]` と一致する
/// クライアントのうち、最後に接続したもの。無ければ `"default"`。
fn current_scene(reg: &[Client], config: &Config) -> String {
    let scene_names: HashSet<&str> = config.scene_names().collect();
    reg.iter()
        .rev()
        .find(|c| scene_names.contains(c.name.as_str()))
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "default".to_string())
}

/// visible 判定に使うクライアント情報(`Client` から UnixStream を除いたもの)。
struct LayerInfo<'a> {
    id: u64,
    name: &'a str,
    session: Option<&'a str>,
    rect: Option<Rect>,
}

/// 全クライアントの visible を計算する。`infos` と同じ順で返す。
///
/// 判定:
/// 1. シーン許可: 名前が `layers` に含まれるか。無ければ visible=false。
/// 2. セッション拘束: 申告セッションがアクティブセッションと一致するか
///    (どちらかが不明なら適用しない = フェイルオープン)。不一致なら
///    visible=false, reason="session"。
/// 3. 重なり調停(クリップ): `layers` の並びを優先度(先頭が最上位)とし、rect を
///    申告したクライアントは visible=true のまま、自分より優先度が高い表示中
///    クライアントの rect と重なる領域を `clip`(描画禁止矩形)として受け取る。
///    クライアントはその矩形を避けて描くのでチカチカしない。同名クライアント
///    同士は後着が上。rect 未申告のクライアントは調停に参加しない(clip 空)。
fn compute_visibility(infos: &[LayerInfo], layers: &[String], active: Option<&str>) -> Vec<Visible> {
    let layer_index = |name: &str| layers.iter().position(|l| l == name);
    let mut order: Vec<usize> = (0..infos.len()).collect();
    order.sort_by_key(|&i| {
        (layer_index(infos[i].name).unwrap_or(usize::MAX), std::cmp::Reverse(infos[i].id))
    });

    let mut out = vec![Visible { visible: false, reason: None, clip: Vec::new() }; infos.len()];
    // 優先度が高い順に処理し、表示中クライアントの rect を積んでいく。
    // 下位クライアントはこの矩形群との重なりを clip として受け取る。
    let mut occluders: Vec<Rect> = Vec::new();
    for &i in &order {
        let c = &infos[i];
        let in_scene = layer_index(c.name).is_some();
        // active_session が取れない間 (tmux 不在等) はセッション拘束を適用しない
        let session_ok = match (c.session, active) {
            (Some(cs), Some(a)) => cs == a,
            _ => true,
        };
        if !in_scene {
            out[i] = Visible { visible: false, reason: None, clip: Vec::new() };
            continue;
        }
        if !session_ok {
            out[i] = Visible { visible: false, reason: Some("session".to_string()), clip: Vec::new() };
            continue;
        }
        let clip: Vec<Rect> = match c.rect {
            Some(r) => occluders.iter().filter_map(|o| o.intersect(&r)).collect(),
            None => Vec::new(),
        };
        out[i] = Visible { visible: true, reason: None, clip };
        if let Some(r) = c.rect {
            occluders.push(r);
        }
    }
    out
}

/// レジストリの現在状態から visible を再計算し、全クライアントへ配信する。
/// あわせて `status-bar` レイヤーの有無に応じて tmux のステータス行を切り替える。
fn recompute_and_broadcast(shared: &Shared, config: &Config) {
    let reg = shared.clients.lock().unwrap();
    let active = shared.active_session.lock().unwrap().clone();
    let scene = current_scene(&reg, config);
    let layers = config.layers_for(&scene);

    eprintln!(
        "[fb-server] scene={scene} layers={layers:?} active_session={active:?} clients={:?}",
        reg.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
    );

    let infos: Vec<LayerInfo> = reg
        .iter()
        .map(|c| LayerInfo {
            id: c.id,
            name: c.name.as_str(),
            session: c.session.as_deref(),
            rect: c.rect,
        })
        .collect();
    let visibles = compute_visibility(&infos, layers, active.as_deref());
    for (c, visible) in reg.iter().zip(&visibles) {
        let mut line = serde_json::to_string(visible).unwrap_or_default();
        line.push('\n');
        let _ = (&c.write).write_all(line.as_bytes());
    }

    let status_on = layers.iter().any(|l| l == "status-bar");
    crate::tmux::run(&["set", "-g", "status", if status_on { "on" } else { "off" }]);
}

fn handle_client(id: u64, stream: UnixStream, shared: Arc<Shared>, config: Arc<Config>) {
    let read_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let hello: Hello = match serde_json::from_str(line.trim()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[fb-server] 不正な hello: {e}");
            return;
        }
    };

    if hello.hello == STATUS_QUERY_NAME {
        let reg = shared.clients.lock().unwrap();
        let scene = current_scene(&reg, &config);
        let clients = reg
            .iter()
            .map(|c| {
                let mut s = c.name.clone();
                if let Some(sess) = &c.session {
                    s.push_str(&format!(" (session {sess})"));
                }
                if let Some(r) = &c.rect {
                    s.push_str(&format!(" [{}x{}+{}+{}]", r.w, r.h, r.x, r.y));
                }
                s
            })
            .collect();
        drop(reg);
        let active_session = shared.active_session.lock().unwrap().clone();
        let reply = StatusReply { scene, clients, active_session };
        let mut out = serde_json::to_string(&reply).unwrap_or_default();
        out.push('\n');
        let _ = (&stream).write_all(out.as_bytes());
        return;
    }

    eprintln!(
        "[fb-server] + client #{id} name={} session={:?} rect={:?}",
        hello.hello, hello.session, hello.rect
    );
    let bound = hello.session.is_some();
    shared.clients.lock().unwrap().push(Client {
        id,
        name: hello.hello.clone(),
        session: hello.session,
        rect: hello.rect,
        write: stream,
    });
    // セッション拘束クライアントの初回判定をポーリングを待たずに行う
    if bound {
        let now = query_active_session();
        *shared.active_session.lock().unwrap() = now;
    }
    recompute_and_broadcast(&shared, &config);

    // 以降は読み続けるだけ。EOF/エラーで切断とみなし登録解除する。
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    shared.clients.lock().unwrap().retain(|c| c.id != id);
    eprintln!("[fb-server] - client #{id} ({})", hello.hello);
    recompute_and_broadcast(&shared, &config);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Option<Rect> {
        Some(Rect { x, y, w, h })
    }

    fn layers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn info(id: u64, name: &str, rect: Option<Rect>) -> LayerInfo<'_> {
        LayerInfo { id, name, session: None, rect }
    }

    fn r(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn overlap_clips_lower_priority_layer() {
        let layers = layers(&["task-var", "spotatui-pip"]);
        let infos = vec![
            info(1, "spotatui-pip", rect(0, 0, 100, 100)),
            info(2, "task-var", rect(50, 50, 100, 100)),
        ];
        let v = compute_visibility(&infos, &layers, None);
        // task-var が配列の先頭 = 優先度が高い。下位 spotatui-pip は消えず、
        // 重なり領域 (50,50)-(100,100) を clip として受け取る。
        assert!(v[0].visible);
        assert_eq!(v[0].clip, vec![r(50, 50, 50, 50)]);
        assert!(v[1].visible);
        assert!(v[1].clip.is_empty(), "最上位に clip は付かない");
    }

    #[test]
    fn disjoint_rects_get_no_clip() {
        let layers = layers(&["task-var", "spotatui-pip"]);
        let infos = vec![
            info(1, "spotatui-pip", rect(0, 0, 50, 50)),
            info(2, "task-var", rect(50, 50, 50, 50)),
        ];
        let v = compute_visibility(&infos, &layers, None);
        assert!(v[0].visible && v[0].clip.is_empty());
        assert!(v[1].visible && v[1].clip.is_empty());
    }

    #[test]
    fn client_without_rect_does_not_participate() {
        let layers = layers(&["task-var", "spotatui-pip"]);
        let infos = vec![
            // 下位が rect 未申告 → clip を受け取らない
            info(1, "spotatui-pip", None),
            info(2, "task-var", rect(0, 0, 100, 100)),
        ];
        let v = compute_visibility(&infos, &layers, None);
        assert!(v[0].visible && v[0].clip.is_empty());
        assert!(v[1].visible && v[1].clip.is_empty());
    }

    #[test]
    fn rectless_upper_layer_does_not_clip_lower() {
        // 上位が rect 未申告なら occluder にならず、下位は clip を受けない
        let layers = layers(&["task-var", "spotatui-pip"]);
        let infos = vec![
            info(1, "spotatui-pip", rect(0, 0, 100, 100)),
            info(2, "task-var", None),
        ];
        let v = compute_visibility(&infos, &layers, None);
        assert!(v[0].visible && v[0].clip.is_empty());
    }

    #[test]
    fn clip_accumulates_from_all_higher_layers() {
        // 下位 c は上位 a, b 双方と重なるので clip を2つ受け取る
        let layers = layers(&["a", "b", "c"]);
        let infos = vec![
            info(1, "a", rect(0, 0, 20, 20)),
            info(2, "b", rect(80, 0, 20, 20)),
            info(3, "c", rect(0, 0, 100, 20)),
        ];
        let v = compute_visibility(&infos, &layers, None);
        assert!(v[2].visible);
        assert_eq!(v[2].clip.len(), 2);
        assert!(v[2].clip.contains(&r(0, 0, 20, 20)));
        assert!(v[2].clip.contains(&r(80, 0, 20, 20)));
    }

    #[test]
    fn out_of_scene_layer_never_clips() {
        let layers = layers(&["b"]);
        let infos = vec![
            info(1, "a", rect(0, 0, 100, 100)),
            info(2, "b", rect(0, 0, 100, 100)),
        ];
        let v = compute_visibility(&infos, &layers, None);
        assert!(!v[0].visible, "シーン外は非表示");
        assert_eq!(v[0].reason, None, "シーン外の非表示に reason は付かない");
        assert!(v[1].visible);
        assert!(v[1].clip.is_empty(), "シーン外の a は occluder にならない");
    }

    #[test]
    fn session_mismatch_hides_and_stops_occluding() {
        // セッション不一致の上位は非表示 → 下位を clip しない
        let layers = layers(&["a", "b"]);
        let mut a = info(1, "a", rect(0, 0, 100, 100));
        a.session = Some("$1");
        let infos = vec![a, info(2, "b", rect(0, 0, 100, 100))];
        let v = compute_visibility(&infos, &layers, Some("$0"));
        assert!(!v[0].visible);
        assert_eq!(v[0].reason.as_deref(), Some("session"));
        assert!(v[1].visible);
        assert!(v[1].clip.is_empty(), "非表示の a は clip 源にならない");
    }

    #[test]
    fn same_name_later_connection_clips_earlier() {
        let layers = layers(&["a"]);
        let infos = vec![
            info(1, "a", rect(0, 0, 10, 10)),
            info(2, "a", rect(0, 0, 10, 10)),
        ];
        let v = compute_visibility(&infos, &layers, None);
        // 後着 (#2) が上位。先着 (#1) は全面 clip される。
        assert!(v[1].visible && v[1].clip.is_empty());
        assert!(v[0].visible);
        assert_eq!(v[0].clip, vec![r(0, 0, 10, 10)]);
    }

    #[test]
    fn intersect_computes_overlap_region() {
        let a = r(0, 0, 50, 50);
        assert_eq!(a.intersect(&r(50, 0, 50, 50)), None, "辺が接するだけは重ならない");
        assert_eq!(a.intersect(&r(40, 40, 50, 50)), Some(r(40, 40, 10, 10)));
        assert_eq!(a.intersect(&r(0, 0, 0, 0)), None, "幅0は重ならない");
        assert_eq!(a.intersect(&r(10, 10, 20, 20)), Some(r(10, 10, 20, 20)), "内包");
    }
}
