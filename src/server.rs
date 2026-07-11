//! デーモン本体。クライアントの接続/切断だけを追跡し、現在のシーンに応じた
//! `visible` を各クライアントへ配信する。ピクセルは一切扱わない。
//!
//! hello で tmux セッションID (`session`) を申告したクライアントは、その
//! セッションがアクティブな間だけ visible=true になる(シーン許可との AND)。
//! アクティブセッションはセッション拘束クライアントが居る間だけ tmux を
//! ポーリングして追跡する。

use crate::protocol::{socket_path, Hello, StatusReply, Visible, STATUS_QUERY_NAME};
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

/// レジストリの現在状態から visible を再計算し、全クライアントへ配信する。
/// あわせて `status-bar` レイヤーの有無に応じて tmux のステータス行を切り替える。
fn recompute_and_broadcast(shared: &Shared, config: &Config) {
    let reg = shared.clients.lock().unwrap();
    let active = shared.active_session.lock().unwrap().clone();
    let scene = current_scene(&reg, config);
    let layers = config.layers_for(&scene);
    let layer_set: HashSet<&str> = layers.iter().map(|s| s.as_str()).collect();

    eprintln!(
        "[fb-server] scene={scene} layers={layers:?} active_session={active:?} clients={:?}",
        reg.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
    );

    for c in reg.iter() {
        let in_scene = layer_set.contains(c.name.as_str());
        // active_session が取れない間 (tmux 不在等) はセッション拘束を適用しない
        let session_ok = match (&c.session, &active) {
            (Some(cs), Some(a)) => cs == a,
            _ => true,
        };
        let visible = Visible {
            visible: in_scene && session_ok,
            reason: (in_scene && !session_ok).then(|| "session".to_string()),
        };
        let mut line = serde_json::to_string(&visible).unwrap_or_default();
        line.push('\n');
        let _ = (&c.write).write_all(line.as_bytes());
    }

    let status_on = layer_set.contains("status-bar");
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
            .map(|c| match &c.session {
                Some(s) => format!("{} (session {s})", c.name),
                None => c.name.clone(),
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
        "[fb-server] + client #{id} name={} session={:?}",
        hello.hello, hello.session
    );
    let bound = hello.session.is_some();
    shared.clients.lock().unwrap().push(Client {
        id,
        name: hello.hello.clone(),
        session: hello.session,
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
