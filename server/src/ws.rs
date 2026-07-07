use crate::term::{Event, SubMsg};
use crate::App;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Frame format (bidirectional, always binary): [type: u8][term_id: u32 LE][payload]
/// type 0: raw terminal bytes (S→C output / C→S input)
/// type 1: JSON (control/events; term_id set to 0)
const FT_BYTES: u8 = 0;
const FT_JSON: u8 = 1;

/// Outbound slice size: prevents any single frame from hogging the connection (app-level muxing fairness).
const SLICE: usize = 32 * 1024;
/// Outbound queue per connection.
const OUT_QUEUE: usize = 256;

fn frame(ft: u8, id: u64, payload: &[u8]) -> Message {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(ft);
    v.extend_from_slice(&(id as u32).to_le_bytes());
    v.extend_from_slice(payload);
    Message::Binary(Bytes::from(v))
}

fn json_frame(value: &serde_json::Value) -> Message {
    frame(FT_JSON, 0, value.to_string().as_bytes())
}

fn event_frame(ev: &Event) -> Message {
    json_frame(&serde_json::to_value(ev).unwrap())
}

/// Origin check: defend against cross-site WebSocket hijacking.
/// Allowed if: Origin host matches the request Host, or is in the configured allowlist.
fn origin_ok(headers: &HeaderMap, app: &App) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false; // browsers always send Origin; reject anything without it
    };
    if app.cfg.allowed_origins.iter().any(|o| o == origin) {
        return true;
    }
    let origin_host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or("");
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    !origin_host.is_empty() && origin_host == host
}

pub async fn ws_handler(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    trusted: Option<axum::Extension<crate::TrustedLocal>>,
) -> Response {
    // Local UNIX-socket clients are pre-trusted (no browser Origin header exists).
    if trusted.is_none() && !origin_ok(&headers, &app) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    ws.on_upgrade(move |sock| conn(sock, app))
}

async fn conn(sock: WebSocket, app: Arc<App>) {
    let (mut ws_tx, mut ws_rx) = sock.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(OUT_QUEUE);

    // Writer task: outbound queue → WS
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if ws_tx.send(m).await.is_err() {
                break;
            }
        }
    });

    // Heartbeat: 30s ping to stop Cloudflare from closing idle connections
    let ping_tx = out_tx.clone();
    let pinger = tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            iv.tick().await;
            if ping_tx.send(Message::Ping(Bytes::new())).await.is_err() {
                break;
            }
        }
    });

    // Global event forwarder
    let mut brx = app.events.subscribe();
    let btx = out_tx.clone();
    let bcaster = tokio::spawn(async move {
        loop {
            match brx.recv().await {
                Ok(ev) => {
                    if btx.send(event_frame(&ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    // hello: current terminal list
    let _ = out_tx
        .send(json_frame(&serde_json::json!({
            "ev": "hello",
            "terminals": app.terms.list(),
        })))
        .await;

    // Main loop: receive client frames
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Binary(b) => {
                if b.len() < 5 {
                    continue;
                }
                let ft = b[0];
                let id = u32::from_le_bytes([b[1], b[2], b[3], b[4]]) as u64;
                let payload = b.slice(5..);
                match ft {
                    FT_BYTES => {
                        app.terms.input(id, payload).await;
                    }
                    FT_JSON => {
                        if let Ok(op) = serde_json::from_slice::<Op>(&payload) {
                            handle_op(&app, &out_tx, op).await;
                        }
                    }
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Cleanup: kill writer task → all forward tasks' sends fail and exit → each detaches itself
    pinger.abort();
    bcaster.abort();
    writer.abort();
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    Create {
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    },
    Attach {
        id: u64,
    },
    Detach {
        id: u64,
        sub: u64,
    },
    Kill {
        id: u64,
    },
    Resize {
        id: u64,
        rows: u16,
        cols: u16,
    },
    List,
}

async fn handle_op(app: &Arc<App>, out_tx: &mpsc::Sender<Message>, op: Op) {
    match op {
        Op::Create { cwd, rows, cols } => match app.terms.create(cwd, rows, cols) {
            Ok(info) => {
                // The Created event is broadcast to all connections; here we also
                // send a targeted copy back to the requester so it knows "this is the one I just opened".
                let _ = out_tx
                    .send(json_frame(&serde_json::json!({
                        "ev": "create_ok",
                        "term": info,
                    })))
                    .await;
            }
            Err(e) => {
                let _ = out_tx
                    .send(json_frame(&serde_json::json!({"ev": "error", "msg": e})))
                    .await;
            }
        },
        Op::Attach { id } => {
            let sub_id = app.terms.next_sub_id();
            let Some((snap, exited, mut rx)) = app.terms.attach(id, sub_id) else {
                let _ = out_tx
                    .send(json_frame(
                        &serde_json::json!({"ev": "error", "msg": format!("terminal {id} not found")}),
                    ))
                    .await;
                return;
            };
            // attached event (client resets the terminal on receipt) → snapshot replay → live stream.
            // All three go through the same outbound queue, so ordering is naturally correct.
            let _ = out_tx
                .send(json_frame(&serde_json::json!({
                    "ev": "attached",
                    "id": id,
                    "sub": sub_id,
                    "exited": exited,
                })))
                .await;
            for chunk in snap.chunks(SLICE) {
                if out_tx.send(frame(FT_BYTES, id, chunk)).await.is_err() {
                    app.terms.detach(id, sub_id);
                    return;
                }
            }
            // Forward task: live stream → outbound queue
            let fwd_tx = out_tx.clone();
            let app2 = app.clone();
            tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    match m {
                        SubMsg::Output { data } => {
                            // attach and vt100 updates are serialized under the same lock,
                            // so bytes received here strictly follow the snapshot.
                            let mut ok = true;
                            for chunk in data.chunks(SLICE) {
                                if fwd_tx.send(frame(FT_BYTES, id, chunk)).await.is_err() {
                                    ok = false;
                                    break;
                                }
                            }
                            if !ok {
                                break;
                            }
                        }
                        SubMsg::Exited => break,
                    }
                }
                app2.terms.detach(id, sub_id);
            });
        }
        Op::Detach { id, sub } => {
            app.terms.detach(id, sub);
        }
        Op::Kill { id } => {
            if !app.terms.kill(id) {
                let _ = out_tx
                    .send(json_frame(
                        &serde_json::json!({"ev": "error", "msg": format!("terminal {id} not found")}),
                    ))
                    .await;
            }
        }
        Op::Resize { id, rows, cols } => {
            app.terms.resize(id, rows.max(2), cols.max(2));
        }
        Op::List => {
            let _ = out_tx
                .send(json_frame(&serde_json::json!({
                    "ev": "hello",
                    "terminals": app.terms.list(),
                })))
                .await;
        }
    }
}
