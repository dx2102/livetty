![Screenshot](docs/screenshot.png)

# livetty

This is a simple web app for driving a remote machine: edit files, run terminals, all in the browser.

The terminals are persistent: they keep running even when the browser tab is closed, until you explicitly close them.

Feature-wise it's similar to JupyterLab, but the backend is written in Rust: smaller memory footprint, snappier UI, fewer glitches.

Pair it with a [cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/) tunnel to reach it over the public internet, behind a password.

## Install

Download the binary for your platform from the [Releases](https://github.com/dx2102/livetty/releases) page and run it:

```bash
curl -L -o livetty https://github.com/dx2102/livetty/releases/latest/download/livetty-linux-x86_64
chmod +x livetty
./livetty serve config.json
```

The server auto-generates `config.json` with a random password (mode 0600) on first run and prints it to the log. Then open `http://localhost:8737` in a browser.

Prebuilt binaries: `linux-x86_64`, `linux-aarch64`, `macos-aarch64` (Apple Silicon).

To reach it from the public internet without owning a domain, install cloudflared and run:

```bash
cloudflared tunnel --url http://localhost:8737
```

Cloudflare will print a `https://<random>.trycloudflare.com` URL. Open it, enter the password from `config.json`, and you're in.

## Features

- Token / password login, hardened for public exposure as best we can (HttpOnly + Secure + SameSite=Strict cookie, constant-time compare, failure rate-limit, WS Origin check to defeat cross-site hijacking)
- Edit files (Monaco, atomic write + mtime conflict detection), use terminals (xterm.js)
- Terminals persist across disconnects and self-heal (server-authoritative state, snapshot replay, no visual garbage)
- Mouse scrolling stays in the browser, so it's instant (unlike a ttyd + tmux setup, where every wheel event round-trips through the server)
- ANSI control sequences never get split mid-stream, so no visual garbage
- All terminals share one WebSocket connection: opening a new terminal is instant, no per-session TCP handshake latency
- tmux/zellij-style local CLI: drive terminals from your shell (spawn new ones, send keystrokes, capture snapshots, list, kill)

## Build from source

**Backend**

```bash
cd server
cargo build --release
./target/release/livetty serve config.json   # auto-generates config.json (random password, mode 0600) on first run
```

The initial password is logged and written to `config.json` (default bind: `127.0.0.1:8737`).

**Frontend**

```bash
cd web
bun install
bun run build        # output goes to web/dist, served by the backend
# dev: bun run dev
```

**Named tunnel (custom domain)**: point a cloudflared named tunnel at `your.domain -> localhost:8737`. The server only binds `127.0.0.1`, so the raw port is never exposed.

## Configuration (`config.json`)

| Field | Description |
|---|---|
| `password` | Login password (auto-generated or set by hand). Startup refuses an empty value. |
| `port` / `bind` | Listen port / address (default 8737 / 127.0.0.1) |
| `allowed_origins` | Extra allowed WS Origins (e.g. the vite dev-server port) |

`config.json` and `sessions.json` contain the password and session tokens; both are `.gitignore`d and never checked in.

