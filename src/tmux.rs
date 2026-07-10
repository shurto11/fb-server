//! tmux コマンド実行の共通ヘルパ。
//!
//! 環境によっては `$TMUX` が指すソケットに辿り着けない(別の /tmp で起動された、
//! ソケットが置かれた領域が unmount/remount された等)。そこで候補ソケットを
//! 列挙して実際に応答するものを一度だけ選び、以降 `-S <socket>` 付きで叩く。
//! serve(router) と tmux-client の両方から使う。

use std::process::Command;
use std::sync::OnceLock;

static SOCKET: OnceLock<Option<String>> = OnceLock::new();

fn debug_on() -> bool {
    std::env::var("TOUCH_DEBUG").map(|v| v != "0" && !v.is_empty()).unwrap_or(false)
}

/// 指定ソケット(None なら bare)で tmux が応答するか。
fn probe(socket: Option<&str>) -> bool {
    let mut cmd = Command::new("tmux");
    if let Some(s) = socket {
        cmd.arg("-S").arg(s);
    }
    cmd.arg("list-sessions");
    matches!(cmd.output(), Ok(o) if o.status.success())
}

/// 稼働中の tmux サーバープロセスの環境から TMUX_TMPDIR を集める(/proc 走査)。
fn tmpdirs_from_proc() -> Vec<String> {
    let mut dirs = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return dirs;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        match std::fs::read_to_string(p.join("comm")) {
            Ok(c) if c.trim_start().starts_with("tmux") => {}
            _ => continue,
        }
        if let Ok(environ) = std::fs::read(p.join("environ")) {
            for kv in environ.split(|&b| b == 0) {
                if let Some(rest) = kv.strip_prefix(b"TMUX_TMPDIR=") {
                    if let Ok(s) = std::str::from_utf8(rest) {
                        if !s.is_empty() {
                            dirs.push(s.to_string());
                        }
                    }
                }
            }
        }
    }
    dirs
}

/// `<base>/tmux-*/` 内のソケットファイルを候補に加える。
fn sockets_in_base(base: &str, out: &mut Vec<String>) {
    if let Ok(rd) = std::fs::read_dir(base) {
        for ent in rd.flatten() {
            if ent.file_name().to_string_lossy().starts_with("tmux-") {
                if let Ok(inner) = std::fs::read_dir(ent.path()) {
                    for s in inner.flatten() {
                        out.push(s.path().to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
}

fn resolve() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();

    // 1. 明示指定
    if let Ok(s) = std::env::var("TOUCH_TMUX_SOCKET") {
        if !s.is_empty() {
            candidates.push(s);
        }
    }
    // 2. $TMUX の先頭フィールド
    if let Ok(t) = std::env::var("TMUX") {
        if let Some(p) = t.split(',').next() {
            if !p.is_empty() {
                candidates.push(p.to_string());
            }
        }
    }
    // 3. tmpdir ベース ($TMUX_TMPDIR / /tmp / 稼働 tmux の TMUX_TMPDIR)
    let mut bases: Vec<String> = Vec::new();
    if let Ok(d) = std::env::var("TMUX_TMPDIR") {
        if !d.is_empty() {
            bases.push(d);
        }
    }
    bases.push("/tmp".to_string());
    bases.extend(tmpdirs_from_proc());
    bases.sort();
    bases.dedup();
    for b in &bases {
        sockets_in_base(b, &mut candidates);
    }
    candidates.dedup();

    // bare tmux が通るならそれを優先 (None = -S 無し)
    if probe(None) {
        if debug_on() {
            eprintln!("[touch] tmux socket: bare (default)");
        }
        return None;
    }
    for c in candidates {
        if probe(Some(&c)) {
            eprintln!("[touch] tmux socket: {c}");
            return Some(c);
        }
    }
    eprintln!("[touch] 警告: 到達可能な tmux ソケットが見つかりません。bare で続行します");
    None
}

fn socket() -> Option<&'static String> {
    SOCKET.get_or_init(resolve).as_ref()
}

/// `-S <socket>`(解決できれば) + 渡された引数。
fn full_args(extra: &[&str]) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(s) = socket() {
        v.push("-S".to_string());
        v.push(s.clone());
    }
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

/// tmux を実行する(出力は捨てる)。失敗は TOUCH_DEBUG 時にログ。
pub fn run(extra: &[&str]) {
    let full = full_args(extra);
    match Command::new("tmux").args(&full).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            if debug_on() {
                eprintln!(
                    "[touch] tmux {:?} 失敗: status={:?} stderr={}",
                    extra,
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
        }
        Err(e) => {
            if debug_on() {
                eprintln!("[touch] tmux {:?} 起動失敗: {e}", extra);
            }
        }
    }
}

/// tmux を実行し標準出力を返す。失敗時は空文字列。
#[allow(dead_code)]
pub fn capture(extra: &[&str]) -> String {
    let full = full_args(extra);
    match Command::new("tmux").args(&full).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            if debug_on() {
                eprintln!(
                    "[touch] tmux {:?} 失敗: status={:?} stderr={}",
                    extra,
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            String::new()
        }
        Err(e) => {
            if debug_on() {
                eprintln!("[touch] tmux {:?} 起動失敗: {e}", extra);
            }
            String::new()
        }
    }
}
