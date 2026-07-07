mod auth;
mod client;
mod files;
mod term;
mod ws;

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{header, StatusCode, Uri},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use clap::{Parser, Subcommand};
use rust_embed::RustEmbed;
use std::sync::Arc;

/// Frontend build output. In release builds rust-embed bakes every file into
/// the binary. In debug builds it reads from disk each request, so editing
/// web/src + `bun run build` + refresh works without a rebuild.
#[derive(RustEmbed)]
#[folder = "../web/dist/"]
struct WebAssets;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub password: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_bind")]
    pub bind: String,
    /// Extra WS Origins allowed beyond same-origin (e.g. the vite dev-server port).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn d_port() -> u16 {
    8737
}
fn d_bind() -> String {
    "127.0.0.1".into()
}

pub struct App {
    pub cfg: Config,
    pub sessions: auth::Sessions,
    pub terms: term::TermManager,
    pub events: tokio::sync::broadcast::Sender<term::Event>,
}

/// Marker inserted into request extensions for requests arriving via the
/// local UNIX socket. Auth and Origin middleware treat these as pre-trusted.
#[derive(Clone, Copy)]
pub struct TrustedLocal;

fn load_or_create_config(path: &str) -> Config {
    if let Ok(s) = std::fs::read_to_string(path) {
        return serde_json::from_str(&s).expect("failed to parse config.json");
    }
    // First run: generate a random password and write config.json (0600)
    use rand::Rng;
    let pw: String = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    let cfg = Config {
        password: pw,
        port: d_port(),
        bind: d_bind(),
        allowed_origins: vec![],
    };
    std::fs::write(path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!("wrote {path}, initial password: {}", cfg.password);
    cfg
}

/// The fixed path for the local control socket. Same for server and client.
pub fn sock_path() -> String {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{}/livetty.sock", dir.trim_end_matches('/'))
    } else {
        let uid = libc_getuid();
        format!("/tmp/livetty-{}.sock", uid)
    }
}

// Small shim so we don't pull in the whole libc crate.
#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}
#[cfg(unix)]
fn libc_getuid() -> u32 {
    unsafe { getuid() }
}
#[cfg(not(unix))]
fn libc_getuid() -> u32 {
    0
}

#[derive(Parser)]
#[command(name = "livetty", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP + WS server (default).
    Serve {
        #[arg(default_value = "config.json")]
        config: String,
    },
    /// List all terminals.
    Ls,
    /// Create a new terminal, print its id.
    New {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(long, default_value_t = 80)]
        cols: u16,
    },
    /// Print the current screen contents (ANSI) for a terminal.
    Snap { id: u64 },
    /// Send raw UTF-8 text as input to a terminal. Use \n, \t, \r escapes.
    Type { id: u64, text: String },
    /// Send special/ctrl keys, comma-separated. Examples: `Enter`, `Tab`, `C-c`, `Up`, `Esc`.
    Keys { id: u64, keys: String },
    /// Kill a terminal (removes it from the list, kills the child process).
    Kill { id: u64 },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Serve {
        config: "config.json".into(),
    }) {
        Cmd::Serve { config } => serve(&config).await,
        Cmd::Ls => client::run(client::Op::Ls).await,
        Cmd::New { cwd, rows, cols } => client::run(client::Op::New { cwd, rows, cols }).await,
        Cmd::Snap { id } => client::run(client::Op::Snap { id }).await,
        Cmd::Type { id, text } => client::run(client::Op::Type { id, text }).await,
        Cmd::Keys { id, keys } => client::run(client::Op::Keys { id, keys }).await,
        Cmd::Kill { id } => client::run(client::Op::Kill { id }).await,
    }
}

/// Serve a request out of the embedded frontend bundle. Falls back to
/// `index.html` for unknown paths so the SPA client-side router works.
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(path).or_else(|| WebAssets::get("index.html")) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(f.data.into_owned()))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn serve(cfg_path: &str) {
    let cfg = load_or_create_config(cfg_path);
    // Basic footgun guard: an empty password would let anyone reaching /api/login
    // in without typing anything. Mirrors sshd's PermitEmptyPasswords=no default.
    if cfg.password.is_empty() {
        eprintln!("refusing to start: password in {cfg_path} is empty");
        std::process::exit(1);
    }

    let (events, _) = tokio::sync::broadcast::channel::<term::Event>(256);
    let app = Arc::new(App {
        sessions: auth::Sessions::load("sessions.json"),
        terms: term::TermManager::new(events.clone()),
        events,
        cfg: cfg.clone(),
    });

    // API router (shared between TCP and UNIX sockets)
    let api = Router::new()
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/me", get(auth::me))
        .route("/api/files", get(files::list_dir))
        .route("/api/file", get(files::read_file).put(files::write_file))
        .route("/api/file/download", get(files::download))
        .route(
            "/api/file/upload",
            post(files::upload).layer(DefaultBodyLimit::max(files::UPLOAD_MAX)),
        )
        .route("/api/fs", post(files::fs_op))
        .route("/ws", get(ws::ws_handler))
        .layer(middleware::from_fn_with_state(app.clone(), auth::auth_mw))
        .with_state(app.clone());

    // TCP: serve API + static SPA fallback (assets baked into the binary)
    let router_tcp = api.clone().fallback(get(static_handler));

    // UNIX: API-only, tagged as TrustedLocal so auth/Origin are skipped
    let router_unix = api.layer(axum::Extension(TrustedLocal));

    let tcp_listener = tokio::net::TcpListener::bind((cfg.bind.as_str(), cfg.port))
        .await
        .expect("failed to bind TCP port");
    tracing::info!("listening on tcp://{}:{}", cfg.bind, cfg.port);

    // Serve UNIX in a spawned task
    #[cfg(unix)]
    {
        let sock = sock_path();
        // Remove stale
        let _ = std::fs::remove_file(&sock);
        // Ensure parent dir exists (it does for XDG_RUNTIME_DIR, but be safe)
        if let Some(parent) = std::path::Path::new(&sock).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let unix_listener =
            tokio::net::UnixListener::bind(&sock).expect("failed to bind UNIX socket");
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
        tracing::info!("listening on unix://{}", sock);

        let router_unix = router_unix.clone();
        tokio::spawn(async move {
            let make_svc = router_unix.into_make_service();
            if let Err(e) = axum::serve(unix_listener, make_svc).await {
                tracing::error!("unix socket serve error: {e}");
            }
        });
    }

    axum::serve(tcp_listener, router_tcp).await.unwrap();
}
