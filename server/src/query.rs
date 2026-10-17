//! Answers to terminal queries (DA, DSR, DECRQM, color queries, ...).
//!
//! Programs ask the terminal questions by printing an escape sequence and
//! reading the answer back from their input. Claude Code and Codex, for
//! example, ask for the background color at startup to pick a light or dark
//! theme. If nobody answers they fall back to dark, and if every open browser
//! tab answers, the extra copies show up as typed garbage.
//!
//! So the server answers from the vt100 state, exactly once, whether zero or
//! many browsers are watching. The browser side swallows these queries (see
//! termpane.ts). Reply formats copy xterm.js 5.5, so programs see what they
//! saw when a browser used to answer.

use std::fmt::Write as _;
use vt100::{MouseProtocolEncoding, MouseProtocolMode, RgbColor, Screen};

/// Same colors as LIGHT_THEME in web/src/termpane.ts. Keep the two in sync.
const FG: RgbColor = rgb(0x000000);
const BG: RgbColor = rgb(0xffffff);
const CURSOR: RgbColor = rgb(0x000000);
const ANSI: [RgbColor; 16] = [
    rgb(0x2e3436),
    rgb(0xcc0000),
    rgb(0x4e9a06),
    rgb(0xc4a000),
    rgb(0x3465a4),
    rgb(0x75507b),
    rgb(0x06989a),
    rgb(0xd3d7cf),
    rgb(0x555753),
    rgb(0xef2929),
    rgb(0x8ae234),
    rgb(0xfce94f),
    rgb(0x729fcf),
    rgb(0xad7fa8),
    rgb(0x34e2e2),
    rgb(0xeeeeec),
];

const fn rgb(hex: u32) -> RgbColor {
    RgbColor {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

/// Default for palette entry `i`: the theme's 16 colors, then the standard
/// xterm 6x6x6 cube and gray ramp (same math as xterm.js).
fn default_indexed(i: u8) -> RgbColor {
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            RgbColor {
                r: level(i / 36),
                g: level(i / 6 % 6),
                b: level(i % 6),
            }
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            RgbColor { r: v, g: v, b: v }
        }
    }
}

/// xterm.js style 16-bit-per-channel color spec, e.g. `rgb:ffff/ffff/ffff`.
fn spec(c: RgbColor) -> String {
    format!(
        "rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}",
        c.r, c.g, c.b
    )
}

/// Reply to a CSI sequence vt100 didn't handle, or None if it isn't a query.
pub fn csi(
    screen: &Screen,
    i1: Option<u8>,
    i2: Option<u8>,
    params: &[&[u16]],
    c: char,
) -> Option<Vec<u8>> {
    let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
    let (row, col) = screen.cursor_position();
    let reply = match (i1, i2, c) {
        // Primary / secondary device attributes.
        (None, None, 'c') if p0 == 0 => "\x1b[?1;2c".to_string(),
        (Some(b'>'), None, 'c') if p0 == 0 => "\x1b[>0;276;0c".to_string(),
        // Device status and cursor position reports.
        (None, None, 'n') if p0 == 5 => "\x1b[0n".to_string(),
        (None, None, 'n') if p0 == 6 => format!("\x1b[{};{}R", row + 1, col + 1),
        (Some(b'?'), None, 'n') if p0 == 6 => format!("\x1b[?{};{}R", row + 1, col + 1),
        // Text area size in characters. xterm.js leaves this off by default,
        // but answering it here is free and always agrees with the pty size.
        (None, None, 't') if p0 == 18 => {
            let (rows, cols) = screen.size();
            format!("\x1b[8;{rows};{cols}t")
        }
        // DECRQM, DEC private and ANSI flavors.
        (Some(b'?'), Some(b'$'), 'p') => format!("\x1b[?{p0};{}$y", private_mode(screen, p0)),
        (Some(b'$'), None, 'p') => format!("\x1b[{p0};{}$y", ansi_mode(screen, p0)),
        _ => return None,
    };
    Some(reply.into_bytes())
}

/// DECRQM value for a DEC private mode: 0 unknown, 1 set, 2 reset,
/// 3 permanently set, 4 permanently reset. Mirrors xterm.js requestMode.
fn private_mode(s: &Screen, mode: u16) -> u8 {
    let on = |b: bool| if b { 1 } else { 2 };
    let mouse = s.mouse_protocol_mode();
    match mode {
        1 => on(s.application_cursor()),
        6 => on(s.origin_mode()),
        7 => on(s.wraparound_mode()),
        8 => 3,
        9 => on(mouse == MouseProtocolMode::Press),
        // vt100 doesn't track cursor blink; xterm.js reports its option,
        // which livetty turns on.
        12 => 1,
        25 => on(!s.hide_cursor()),
        45 => on(s.reverse_wraparound_mode()),
        66 => on(s.application_keypad()),
        67 => 4,
        1000 => on(mouse == MouseProtocolMode::PressRelease),
        1002 => on(mouse == MouseProtocolMode::ButtonMotion),
        1003 => on(mouse == MouseProtocolMode::AnyMotion),
        1004 => on(s.send_focus_mode()),
        1005 | 1015 => 4,
        1006 => on(s.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr),
        1016 => 2,
        1048 => 1,
        47 | 1047 | 1049 => on(s.alternate_screen()),
        2004 => on(s.bracketed_paste()),
        _ => 0,
    }
}

/// DECRQM value for an ANSI mode. Mirrors xterm.js requestMode.
fn ansi_mode(s: &Screen, mode: u16) -> u8 {
    match mode {
        2 => 4,
        4 => {
            if s.insert_mode() {
                1
            } else {
                2
            }
        }
        12 => 3,
        20 => 2,
        _ => 0,
    }
}

/// Reply to a color query (OSC 4/10/11/12 with `?`), or None if `params`
/// holds no query. Colors a program set itself (vt100 tracks them) win over
/// the theme, like in xterm.js. Replies always end in ST, also like xterm.js.
pub fn osc(screen: &Screen, params: &[&[u8]]) -> Option<Vec<u8>> {
    let (first, rest) = params.split_first()?;
    let n: u16 = std::str::from_utf8(first).ok()?.parse().ok()?;
    let pal = screen.palette();
    let mut out = String::new();
    match n {
        4 => {
            for pair in rest.chunks(2) {
                let [idx, q] = pair else { continue };
                if *q != b"?" {
                    continue;
                }
                let Some(i) = std::str::from_utf8(idx).ok().and_then(|s| s.parse::<u8>().ok())
                else {
                    continue;
                };
                let c = pal.indexed(i).unwrap_or_else(|| default_indexed(i));
                write!(out, "\x1b]4;{i};{}\x1b\\", spec(c)).unwrap();
            }
        }
        // `OSC 10;?;?` asks for 10 and then 11, as in xterm.
        10..=12 => {
            for (k, q) in rest.iter().enumerate() {
                let id = n + k as u16;
                if id > 12 {
                    break;
                }
                if *q != b"?" {
                    continue;
                }
                let c = match id {
                    10 => pal.foreground().unwrap_or(FG),
                    11 => pal.background().unwrap_or(BG),
                    _ => pal.cursor().unwrap_or(CURSOR),
                };
                write!(out, "\x1b]{id};{}\x1b\\", spec(c)).unwrap();
            }
        }
        _ => return None,
    }
    (!out.is_empty()).then(|| out.into_bytes())
}
