//! デーモン本体。クライアントの接続/切断だけを追跡し、現在のシーンに応じた
//! `visible` を各クライアントへ配信する。ピクセルは一切扱わない。

use crate::protocol::{socket_path, Hello, StatusReply, Visible, STATUS_QUERY_NAME};
use crate::scenes::Config;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

struct Client {
    id: u64,
    name: String,
    write: UnixStream,
}

type Registry = Arc<Mutex<Vec<Client>>>;

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

    let registry: Registry = Arc::new(Mutex::new(Vec::new()));
    let ids = AtomicU64::new(1);

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let id = ids.fetch_add(1, Ordering::Relaxed);
        let registry = registry.clone();
        let config = config.clone();
        thread::spawn(move || handle_client(id, stream, registry, config));
    }
    Ok(())
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
fn recompute_and_broadcast(registry: &Registry, config: &Config) {
    let reg = registry.lock().unwrap();
    let scene = current_scene(&reg, config);
    let layers = config.layers_for(&scene);
    let layer_set: HashSet<&str> = layers.iter().map(|s| s.as_str()).collect();

    eprintln!(
        "[fb-server] scene={scene} layers={layers:?} clients={:?}",
        reg.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
    );

    for c in reg.iter() {
        let visible = Visible { visible: layer_set.contains(c.name.as_str()) };
        let mut line = serde_json::to_string(&visible).unwrap_or_default();
        line.push('\n');
        let _ = (&c.write).write_all(line.as_bytes());
    }

    let status_on = layer_set.contains("status-bar");
    crate::tmux::run(&["set", "-g", "status", if status_on { "on" } else { "off" }]);
}

fn handle_client(id: u64, stream: UnixStream, registry: Registry, config: Arc<Config>) {
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
        let reg = registry.lock().unwrap();
        let scene = current_scene(&reg, &config);
        let clients = reg.iter().map(|c| c.name.clone()).collect();
        drop(reg);
        let reply = StatusReply { scene, clients };
        let mut out = serde_json::to_string(&reply).unwrap_or_default();
        out.push('\n');
        let _ = (&stream).write_all(out.as_bytes());
        return;
    }

    eprintln!("[fb-server] + client #{id} name={}", hello.hello);
    registry.lock().unwrap().push(Client { id, name: hello.hello.clone(), write: stream });
    recompute_and_broadcast(&registry, &config);

    // 以降は読み続けるだけ。EOF/エラーで切断とみなし登録解除する。
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    registry.lock().unwrap().retain(|c| c.id != id);
    eprintln!("[fb-server] - client #{id} ({})", hello.hello);
    recompute_and_broadcast(&registry, &config);
}
