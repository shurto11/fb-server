//! fb-server: フレームバッファへの書き込みを許可されたクライアントだけに
//! 絞るための調停デーモン。
//!
//!   fb-server serve    デーモン(scenes.toml 読み込み + socket 待受)
//!   fb-server status   デバッグ用: 現在のシーン名 + 接続クライアント一覧

mod protocol;
mod scenes;
mod server;
mod tmux;

use anyhow::{Context, Result};
use protocol::{socket_path, Hello, StatusReply, STATUS_QUERY_NAME};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "serve" => server::serve(),
        "status" => status(),
        other => {
            if !other.is_empty() {
                eprintln!("不明なサブコマンド: {other}");
            }
            eprintln!("使い方: fb-server <serve|status>");
            std::process::exit(2);
        }
    }
}

fn status() -> Result<()> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("fb-server に接続できません ({path})"))?;
    let hello = Hello { hello: STATUS_QUERY_NAME.to_string(), session: None, rect: None };
    let mut line = serde_json::to_string(&hello)?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;

    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    let reply: StatusReply = serde_json::from_str(resp.trim())?;

    println!("scene: {}", reply.scene);
    if let Some(s) = &reply.active_session {
        println!("active tmux session: {s}");
    }
    if reply.clients.is_empty() {
        println!("clients: (none)");
    } else {
        println!("clients:");
        for c in &reply.clients {
            println!("  - {c}");
        }
    }
    Ok(())
}
