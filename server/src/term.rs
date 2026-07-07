use bytes::Bytes;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

/// Per-subscriber frame queue depth; a full queue blocks the PTY read thread, natural backpressure.
const SUB_QUEUE: usize = 64;
const READ_BUF: usize = 16 * 1024;
/// How many rows of scrollback the vt100 parser keeps. Every attach dumps the whole
/// buffer as an ANSI replay script for xterm.js to rebuild its own scrollback.
const SCROLLBACK_ROWS: usize = 5000;

// ---------- Global events (broadcast to every WS connection) ----------

#[derive(Clone, serde::Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    Created { term: TermInfo },
    Removed { id: u64 },
    Exited { id: u64 },
    Title { id: u64, title: String },
    Resized { id: u64, rows: u16, cols: u16 },
}

#[derive(Clone, serde::Serialize)]
pub struct TermInfo {
    pub id: u64,
    pub title: String,
    pub rows: u16,
    pub cols: u16,
    pub exited: bool,
}

// ---------- Subscriber messages ----------

pub enum SubMsg {
    Output { data: Bytes },
    Exited,
}

// ---------- vt100 callbacks: capture window title + log unhandled sequences ----------

#[derive(Default)]
struct Cb {
    title: Option<String>,
    title_dirty: bool,
    /// Signatures of unhandled sequences already logged. Kept per-terminal so
    /// each new terminal re-logs anything it hits, useful when a specific
    /// program is the source and you want confirmation.
    seen_unhandled: HashSet<String>,
}

impl Cb {
    fn note_unhandled(&mut self, sig: String) {
        if self.seen_unhandled.insert(sig.clone()) {
            tracing::warn!(target: "vt100", "unhandled sequence: {sig}");
        }
    }
}

impl vt100::Callbacks for Cb {
    fn set_window_title(&mut self, _: &mut vt100::Screen, t: &[u8]) {
        self.title = Some(String::from_utf8_lossy(t).into_owned());
        self.title_dirty = true;
    }
    fn unhandled_char(&mut self, _: &mut vt100::Screen, c: char) {
        self.note_unhandled(format!("char {:?} (U+{:04X})", c, c as u32));
    }
    fn unhandled_control(&mut self, _: &mut vt100::Screen, b: u8) {
        self.note_unhandled(format!("control 0x{b:02x}"));
    }
    fn unhandled_escape(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        b: u8,
    ) {
        self.note_unhandled(format!(
            "ESC {}{}{}",
            i1.map(|c| (c as char).to_string()).unwrap_or_default(),
            i2.map(|c| (c as char).to_string()).unwrap_or_default(),
            b as char,
        ));
    }
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let params_str: Vec<String> = params
            .iter()
            .map(|p| {
                p.iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .collect();
        self.note_unhandled(format!(
            "CSI {}{}{}{}",
            i1.map(|c| (c as char).to_string()).unwrap_or_default(),
            i2.map(|c| (c as char).to_string()).unwrap_or_default(),
            params_str.join(";"),
            c,
        ));
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let joined: Vec<String> = params
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        self.note_unhandled(format!("OSC {}", joined.join(";")));
    }
}

// ---------- Terminal state (inside the mutex) ----------

struct TermState {
    parser: vt100::Parser<Cb>,
    subs: HashMap<u64, mpsc::Sender<SubMsg>>,
    exited: bool,
    rows: u16,
    cols: u16,
}

impl TermState {
    /// Serialize the vt100 parser's authoritative state (scrollback + current
    /// visible screen + attributes + input mode + cursor) into an ANSI byte
    /// stream that xterm.js can consume to reproduce the exact same picture ,
    /// including populating its own scrollback buffer.
    ///
    /// This is the only "snapshot" function. Called from `attach`. Never called
    /// on the hot PTY-read path.
    ///
    /// Output is derived purely from vt100's structured Cell state, so it's
    /// guaranteed not to contain raw device-query sequences (DA/DSR/OSC-query
    /// etc.), vt100 wouldn't emit them.
    fn dump(&self) -> Vec<u8> {
        use std::io::Write as _;
        let mut out = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(b"\x1bc"); // RIS: full reset
        // Piebald fork's `state_formatted_full` handles: alt-screen, scrollback
        // rows, current screen, cursor state, attributes, input mode, origin
        // mode, scroll region, hyperlinks. Everything.
        let scr = self.parser.screen();
        scr.write_state_formatted_full(&mut out);
        // Piebald fork bug workaround: on the main screen it ends with a CUP
        // whose row = viewport_row + scrollback_rows_len (an absolute row).
        // xterm.js treats CUP as viewport-relative and clamps rows > screen
        // height to the bottom line, so the cursor ends up 1-N rows lower than
        // it should, visible in TUIs like claude-code where the input caret
        // sits a couple of rows above the bottom. Re-emit a correct CUP so it
        // overrides the earlier one. hyperlink/input_mode bytes emitted after
        // it don't touch cursor position, so this is the last word.
        let (cur_row, cur_col) = scr.cursor_position();
        let row_1based = if scr.origin_mode() {
            let (scroll_top, _) = scr.scroll_region();
            cur_row.saturating_sub(scroll_top)
        } else {
            cur_row
        } + 1;
        write!(&mut out, "\x1b[{};{}H", row_1based, cur_col + 1).unwrap();
        out
    }
}

// ---------- Single terminal ----------

pub struct Terminal {
    pub id: u64,
    state: Arc<Mutex<TermState>>,
    /// Input goes through a dedicated writer thread (bounded queue) so a full PTY input buffer doesn't stall the async runtime.
    input_tx: mpsc::Sender<Bytes>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl Terminal {
    pub fn info(&self) -> TermInfo {
        let st = self.state.lock().unwrap();
        TermInfo {
            id: self.id,
            title: st.parser.callbacks().title.clone().unwrap_or_default(),
            rows: st.rows,
            cols: st.cols,
            exited: st.exited,
        }
    }
}

// ---------- Manager ----------

pub struct TermManager {
    terms: Mutex<HashMap<u64, Arc<Terminal>>>,
    next_id: AtomicU64,
    next_sub: AtomicU64,
    events: broadcast::Sender<Event>,
}

impl TermManager {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        TermManager {
            terms: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_sub: AtomicU64::new(1),
            events,
        }
    }

    pub fn next_sub_id(&self) -> u64 {
        self.next_sub.fetch_add(1, Ordering::Relaxed)
    }

    pub fn list(&self) -> Vec<TermInfo> {
        let mut v: Vec<TermInfo> = self
            .terms
            .lock()
            .unwrap()
            .values()
            .map(|t| t.info())
            .collect();
        v.sort_by_key(|t| t.id);
        v
    }

    fn get(&self, id: u64) -> Option<Arc<Terminal>> {
        self.terms.lock().unwrap().get(&id).cloned()
    }

    pub fn create(
        &self,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) -> anyhow_lite::Result<TermInfo> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        cmd.cwd(cwd.unwrap_or(home));

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn: {e}"))?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone_reader: {e}"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take_writer: {e}"))?;

        let state = Arc::new(Mutex::new(TermState {
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_ROWS, Cb::default()),
            subs: HashMap::new(),
            exited: false,
            rows,
            cols,
        }));

        // Writer thread for input
        let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(256);
        std::thread::spawn(move || {
            while let Some(data) = input_rx.blocking_recv() {
                if writer.write_all(&data).is_err() {
                    break;
                }
            }
        });

        // PTY reader thread: feed chunks straight into vt100 + fan out raw bytes to
        // active subscribers. No buffer accumulation, the vt100 parser IS the storage.
        let rstate = state.clone();
        let revents = self.events.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let chunk = &buf[..n];
                let (senders, title);
                {
                    let mut st = rstate.lock().unwrap();
                    st.parser.process(chunk);
                    let cb = st.parser.callbacks_mut();
                    title = if cb.title_dirty {
                        cb.title_dirty = false;
                        cb.title.clone()
                    } else {
                        None
                    };
                    senders = st
                        .subs
                        .iter()
                        .map(|(k, v)| (*k, v.clone()))
                        .collect::<Vec<_>>();
                }
                if let Some(t) = title {
                    let _ = revents.send(Event::Title { id, title: t });
                }
                let data = Bytes::copy_from_slice(chunk);
                for (sid, tx) in senders {
                    // Subscriber queue full → block here → stop reading PTY → kernel buffer fills
                    // → the program inside the terminal blocks on write. Never drop bytes.
                    if tx
                        .blocking_send(SubMsg::Output { data: data.clone() })
                        .is_err()
                    {
                        rstate.lock().unwrap().subs.remove(&sid);
                    }
                }
            }
            // EOF: child process exited
            let senders;
            {
                let mut st = rstate.lock().unwrap();
                st.exited = true;
                senders = st.subs.values().cloned().collect::<Vec<_>>();
            }
            for tx in senders {
                let _ = tx.blocking_send(SubMsg::Exited);
            }
            let _ = revents.send(Event::Exited { id });
        });

        // Reaper thread
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        let term = Arc::new(Terminal {
            id,
            state,
            input_tx,
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
        });
        let info = term.info();
        self.terms.lock().unwrap().insert(id, term);
        let _ = self.events.send(Event::Created { term: info.clone() });
        Ok(info)
    }

    /// attach: take snapshot and register subscriber inside the same lock, the seam
    /// is naturally gap-free and duplicate-free.
    pub fn attach(
        &self,
        id: u64,
        sub_id: u64,
    ) -> Option<(Vec<u8>, bool, mpsc::Receiver<SubMsg>)> {
        let term = self.get(id)?;
        let (tx, rx) = mpsc::channel(SUB_QUEUE);
        let mut st = term.state.lock().unwrap();
        let snap = st.dump();
        let exited = st.exited;
        st.subs.insert(sub_id, tx);
        Some((snap, exited, rx))
    }

    pub fn detach(&self, id: u64, sub_id: u64) {
        if let Some(term) = self.get(id) {
            term.state.lock().unwrap().subs.remove(&sub_id);
        }
    }

    pub async fn input(&self, id: u64, data: Bytes) -> bool {
        match self.get(id) {
            Some(t) => t.input_tx.send(data).await.is_ok(),
            None => false,
        }
    }

    pub fn resize(&self, id: u64, rows: u16, cols: u16) -> bool {
        let Some(term) = self.get(id) else {
            return false;
        };
        {
            let mut st = term.state.lock().unwrap();
            st.parser.screen_mut().set_size(rows, cols);
            st.rows = rows;
            st.cols = cols;
        }
        let ok = term
            .master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok();
        if ok {
            let _ = self.events.send(Event::Resized { id, rows, cols });
        }
        ok
    }

    /// kill = kill the process + remove from list (i.e. "close the terminal")
    pub fn kill(&self, id: u64) -> bool {
        let Some(term) = self.terms.lock().unwrap().remove(&id) else {
            return false;
        };
        let _ = term.killer.lock().unwrap().kill();
        let _ = self.events.send(Event::Removed { id });
        true
    }
}

// Minimal Result alias to avoid pulling in anyhow
pub mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, String>;
}
