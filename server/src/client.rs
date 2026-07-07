//! CLI client. Talks WS-only to the local server via UNIX socket.
//!
//! Reuses the same custom binary frame protocol as the browser:
//!   [type: u8][term_id: u32 LE][payload]
//!   type 0 = raw terminal bytes
//!   type 1 = JSON control/events
//!
//! For each subcommand we open a WS, do the minimum interaction, and exit.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

const FT_BYTES: u8 = 0;
const FT_JSON: u8 = 1;

pub enum Op {
    Ls,
    New {
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    },
    Snap {
        id: u64,
    },
    Type {
        id: u64,
        text: String,
    },
    Keys {
        id: u64,
        keys: String,
    },
    Kill {
        id: u64,
    },
}

// ---------- frame helpers ----------

fn frame(ft: u8, term_id: u64, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(ft);
    v.extend_from_slice(&(term_id as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn json_frame(value: &Value) -> Vec<u8> {
    frame(FT_JSON, 0, value.to_string().as_bytes())
}

fn bytes_frame(term_id: u64, data: &[u8]) -> Vec<u8> {
    frame(FT_BYTES, term_id, data)
}

// ---------- connection ----------

type Ws = tokio_tungstenite::WebSocketStream<UnixStream>;

async fn connect() -> Ws {
    let sock_path = crate::sock_path();
    let stream = UnixStream::connect(&sock_path).await.unwrap_or_else(|e| {
        eprintln!(
            "wt: cannot connect to {}: {}\n\
             (is the server running? try: livetty serve config.json)",
            sock_path, e
        );
        std::process::exit(1);
    });
    // We're doing WebSocket over an already-open stream. tungstenite still needs
    // a URI/request to produce the HTTP upgrade; the host is arbitrary since we
    // don't route on it.
    let req = "ws://localhost/ws".into_client_request().unwrap();
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .unwrap_or_else(|e| {
            eprintln!("wt: WS handshake failed: {e}");
            std::process::exit(1);
        });
    ws
}

async fn send_json(ws: &mut Ws, v: &Value) {
    ws.send(Message::Binary(json_frame(v).into())).await.unwrap();
}

async fn send_bytes(ws: &mut Ws, term_id: u64, data: &[u8]) {
    ws.send(Message::Binary(bytes_frame(term_id, data).into()))
        .await
        .unwrap();
}

/// Read next JSON event, ignoring FT_BYTES. Returns None on close/timeout.
async fn next_json(ws: &mut Ws, timeout: Duration) -> Option<Value> {
    loop {
        let msg = tokio::time::timeout(timeout, ws.next()).await.ok()??;
        let msg = msg.ok()?;
        if let Message::Binary(b) = msg {
            if b.len() < 5 {
                continue;
            }
            if b[0] == FT_JSON {
                if let Ok(v) = serde_json::from_slice::<Value>(&b[5..]) {
                    return Some(v);
                }
            }
        }
    }
}

// ---------- op handlers ----------

pub async fn run(op: Op) {
    match op {
        Op::Ls => ls().await,
        Op::New { cwd, rows, cols } => new_term(cwd, rows, cols).await,
        Op::Snap { id } => snap(id).await,
        Op::Type { id, text } => type_text(id, text).await,
        Op::Keys { id, keys: ks } => keys_cmd(id, ks).await,
        Op::Kill { id } => kill(id).await,
    }
}

async fn ls() {
    let mut ws = connect().await;
    // Server sends "hello" immediately on connect with the terminal list.
    let ev = next_json(&mut ws, Duration::from_secs(2)).await;
    let _ = ws.close(None).await;
    let Some(ev) = ev else {
        eprintln!("wt: no hello from server");
        std::process::exit(1);
    };
    let arr = ev.get("terminals").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("(no terminals)");
        return;
    }
    println!("{:>4}  {:<50}  {:>6}  {}", "ID", "TITLE", "SIZE", "STATUS");
    for t in arr {
        let id = t.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let rows = t.get("rows").and_then(|v| v.as_u64()).unwrap_or(0);
        let cols = t.get("cols").and_then(|v| v.as_u64()).unwrap_or(0);
        let exited = t.get("exited").and_then(|v| v.as_bool()).unwrap_or(false);
        let status = if exited { "exited" } else { "alive" };
        let title = if title.len() > 50 { &title[..50] } else { title };
        println!("{:>4}  {:<50}  {:>3}x{:<2}  {}", id, title, rows, cols, status);
    }
}

async fn new_term(cwd: Option<String>, rows: u16, cols: u16) {
    let mut ws = connect().await;
    // Skip hello
    let _ = next_json(&mut ws, Duration::from_secs(2)).await;
    let mut payload = json!({"op": "create", "rows": rows, "cols": cols});
    if let Some(c) = cwd {
        payload["cwd"] = Value::String(c);
    }
    send_json(&mut ws, &payload).await;
    // Wait for create_ok (may also see 'created' broadcast first, accept either)
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut new_id: Option<u64> = None;
    while std::time::Instant::now() < deadline {
        let Some(ev) = next_json(&mut ws, Duration::from_secs(2)).await else {
            break;
        };
        let ev_name = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
        if ev_name == "create_ok" || ev_name == "created" {
            if let Some(id) = ev.get("term").and_then(|t| t.get("id")).and_then(|v| v.as_u64()) {
                new_id = Some(id);
                break;
            }
        } else if ev_name == "error" {
            eprintln!(
                "wt: {}",
                ev.get("msg").and_then(|v| v.as_str()).unwrap_or("create failed")
            );
            let _ = ws.close(None).await;
            std::process::exit(1);
        }
    }
    let _ = ws.close(None).await;
    if let Some(id) = new_id {
        println!("{}", id);
    } else {
        eprintln!("wt: create timed out waiting for create_ok");
        std::process::exit(1);
    }
}

async fn snap(id: u64) {
    let mut ws = connect().await;
    // Skip hello
    let _ = next_json(&mut ws, Duration::from_secs(2)).await;
    send_json(&mut ws, &json!({"op": "attach", "id": id})).await;

    // Then collect FT_BYTES frames for our id until stream goes idle.
    let mut buf = Vec::new();
    let mut seen_attached = false;
    let mut error: Option<String> = None;
    loop {
        let msg = tokio::time::timeout(Duration::from_millis(600), ws.next()).await;
        match msg {
            Err(_) => break, // idle timeout → snapshot done
            Ok(None) => break,
            Ok(Some(Err(_))) => break,
            Ok(Some(Ok(Message::Binary(b)))) => {
                if b.len() < 5 {
                    continue;
                }
                let ft = b[0];
                let term_id = u32::from_le_bytes([b[1], b[2], b[3], b[4]]) as u64;
                if ft == FT_JSON {
                    if let Ok(v) = serde_json::from_slice::<Value>(&b[5..]) {
                        let name = v.get("ev").and_then(|v| v.as_str()).unwrap_or("");
                        if name == "attached" {
                            seen_attached = true;
                        } else if name == "error" {
                            error = Some(
                                v.get("msg")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("attach error")
                                    .to_string(),
                            );
                            break;
                        }
                    }
                } else if ft == FT_BYTES && term_id == id {
                    buf.extend_from_slice(&b[5..]);
                }
            }
            Ok(Some(Ok(_))) => {} // ping/pong/close, ignore
        }
    }
    let _ = ws.close(None).await;
    if let Some(e) = error {
        eprintln!("wt: {e}");
        std::process::exit(1);
    }
    if !seen_attached {
        eprintln!("wt: never received attached event");
        std::process::exit(1);
    }
    // stdout raw bytes (ANSI included)
    let _ = std::io::stdout().write_all(&buf);
    let _ = std::io::stdout().flush();
}

async fn type_text(id: u64, text: String) {
    let mut ws = connect().await;
    let _ = next_json(&mut ws, Duration::from_secs(2)).await;
    // Interpret common escapes so `wt type 3 "ls\n"` works.
    let bytes = unescape(&text);
    send_bytes(&mut ws, id, &bytes).await;
    // Give it a moment to flush over the socket before closing.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = ws.close(None).await;
}

async fn keys_cmd(id: u64, keys: String) {
    let mut ws = connect().await;
    let _ = next_json(&mut ws, Duration::from_secs(2)).await;
    let mut buf = Vec::new();
    for k in keys.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match render_key(k) {
            Some(seq) => buf.extend_from_slice(&seq),
            None => {
                eprintln!("wt: unknown key '{k}'");
                let _ = ws.close(None).await;
                std::process::exit(1);
            }
        }
    }
    send_bytes(&mut ws, id, &buf).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = ws.close(None).await;
}

async fn kill(id: u64) {
    let mut ws = connect().await;
    let _ = next_json(&mut ws, Duration::from_secs(2)).await;
    send_json(&mut ws, &json!({"op": "kill", "id": id})).await;
    // Wait briefly for 'removed' or 'error'
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let Some(ev) = next_json(&mut ws, Duration::from_millis(600)).await else {
            break;
        };
        let name = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
        if name == "removed"
            && ev.get("id").and_then(|v| v.as_u64()) == Some(id)
        {
            break;
        }
        if name == "error" {
            eprintln!(
                "wt: {}",
                ev.get("msg").and_then(|v| v.as_str()).unwrap_or("kill failed")
            );
            let _ = ws.close(None).await;
            std::process::exit(1);
        }
    }
    let _ = ws.close(None).await;
}

// ---------- helpers ----------

/// Expand a small set of C-style escapes (\n \r \t \\ \0 \x1b \e) in `s`.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match it.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('0') => out.push(0),
            Some('e') => out.push(0x1b),
            Some('x') => {
                // \xHH
                let h1 = it.next().unwrap_or('0');
                let h2 = it.next().unwrap_or('0');
                let hex: String = [h1, h2].iter().collect();
                let byte = u8::from_str_radix(&hex, 16).unwrap_or(b'?');
                out.push(byte);
            }
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
}

/// Render a named key or key combo into the byte sequence a terminal expects.
fn render_key(k: &str) -> Option<Vec<u8>> {
    let esc = 0x1bu8;
    // Ctrl combos: C-x
    if let Some(rest) = k.strip_prefix("C-").or_else(|| k.strip_prefix("Ctrl-")) {
        if rest.chars().count() == 1 {
            let c = rest.chars().next().unwrap().to_ascii_lowercase();
            if ('a'..='z').contains(&c) {
                return Some(vec![(c as u8) - b'a' + 1]);
            }
            // Common non-letter ctrl mappings
            return match c {
                '@' | ' ' => Some(vec![0]),
                '[' => Some(vec![esc]),
                '\\' => Some(vec![0x1c]),
                ']' => Some(vec![0x1d]),
                '^' => Some(vec![0x1e]),
                '_' => Some(vec![0x1f]),
                '?' => Some(vec![0x7f]),
                _ => None,
            };
        }
    }
    // Alt / Meta: M-x  →  ESC + x
    if let Some(rest) = k.strip_prefix("M-").or_else(|| k.strip_prefix("Alt-")) {
        if let Some(mut inner) = render_key(rest).or_else(|| Some(rest.as_bytes().to_vec())) {
            let mut out = vec![esc];
            out.append(&mut inner);
            return Some(out);
        }
    }
    match k {
        "Enter" | "Return" | "CR" => Some(vec![b'\r']),
        "Tab" => Some(vec![b'\t']),
        "Backspace" | "BS" => Some(vec![0x7f]),
        "Space" => Some(vec![b' ']),
        "Esc" | "Escape" => Some(vec![esc]),
        "Up" => Some(vec![esc, b'[', b'A']),
        "Down" => Some(vec![esc, b'[', b'B']),
        "Right" => Some(vec![esc, b'[', b'C']),
        "Left" => Some(vec![esc, b'[', b'D']),
        "Home" => Some(vec![esc, b'[', b'H']),
        "End" => Some(vec![esc, b'[', b'F']),
        "PgUp" | "PageUp" => Some(vec![esc, b'[', b'5', b'~']),
        "PgDn" | "PageDown" => Some(vec![esc, b'[', b'6', b'~']),
        "Delete" | "Del" => Some(vec![esc, b'[', b'3', b'~']),
        // Function keys F1-F12
        "F1" => Some(vec![esc, b'O', b'P']),
        "F2" => Some(vec![esc, b'O', b'Q']),
        "F3" => Some(vec![esc, b'O', b'R']),
        "F4" => Some(vec![esc, b'O', b'S']),
        "F5" => Some(vec![esc, b'[', b'1', b'5', b'~']),
        "F6" => Some(vec![esc, b'[', b'1', b'7', b'~']),
        "F7" => Some(vec![esc, b'[', b'1', b'8', b'~']),
        "F8" => Some(vec![esc, b'[', b'1', b'9', b'~']),
        "F9" => Some(vec![esc, b'[', b'2', b'0', b'~']),
        "F10" => Some(vec![esc, b'[', b'2', b'1', b'~']),
        "F11" => Some(vec![esc, b'[', b'2', b'3', b'~']),
        "F12" => Some(vec![esc, b'[', b'2', b'4', b'~']),
        _ => None,
    }
}
