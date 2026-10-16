//! Optional status feeds, fetched on a worker thread so the UI never blocks:
//!   print_url   - the bedswap agent's printer status (JSON)
//!   orders_url  - the House of Madese order summary (JSON)
//! Both are off unless a URL is set in config.ini. Requests go through the
//! `curl.exe` that ships with Windows 10/11 (so https and proxies just work),
//! with the secret passed on stdin rather than on the command line. Nothing runs
//! at all while both URLs are empty.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::store;

/// Posted to the UI window whenever a feed changed.
pub const WM_NET: u32 = 0x8000 + 10; // WM_APP + 10

const PRINT_EVERY: Duration = Duration::from_secs(15);
const ORDERS_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Print {
    pub state: String,
    pub progress: i32,
    pub remaining_min: i32,
    pub job: String,
    pub nozzle: i32,
    pub bed: i32,
    pub layer: i32,
    pub total_layers: i32,
    pub needs_confirmation: bool,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Orders {
    pub total: i64,
    pub today: i64,
    pub latest_at: String,
    pub latest_total: f64,
}

#[derive(Default)]
pub struct NetState {
    pub print: Option<Print>,
    pub print_err: Option<String>,
    pub orders: Option<Orders>,
    pub orders_err: Option<String>,
}

pub struct Handle {
    pub state: Arc<Mutex<NetState>>,
    /// Ask for an immediate refresh (e.g. when the Status page opens).
    pub tx: Sender<()>,
}

pub fn start(hwnd: isize) -> Handle {
    let state = Arc::new(Mutex::new(NetState::default()));
    let (tx, rx) = channel::<()>();
    let st = state.clone();
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let mut next_print = Instant::now();
            let mut next_orders = Instant::now();
            loop {
                let cfg = store::load_config();
                let mut sleep = Duration::from_secs(60);
                let now = Instant::now();
                let mut changed = false;
                if !cfg.print_url.is_empty() {
                    if now >= next_print {
                        let r = fetch(&cfg.print_url, "").and_then(|b| parse_print(&b));
                        let mut s = st.lock().unwrap();
                        match r {
                            Ok(p) => {
                                changed |= s.print.as_ref() != Some(&p) || s.print_err.is_some();
                                s.print = Some(p);
                                s.print_err = None;
                            }
                            Err(e) => {
                                changed |= s.print_err.as_deref() != Some(&e);
                                s.print_err = Some(e);
                            }
                        }
                        next_print = Instant::now() + PRINT_EVERY;
                    }
                    sleep = sleep.min(next_print.saturating_duration_since(Instant::now()));
                } else {
                    let mut s = st.lock().unwrap();
                    if s.print.is_some() || s.print_err.is_some() {
                        s.print = None;
                        s.print_err = None;
                        changed = true;
                    }
                }
                if !cfg.orders_url.is_empty() {
                    if now >= next_orders {
                        let r = fetch(&cfg.orders_url, &cfg.orders_key).and_then(|b| parse_orders(&b));
                        let mut s = st.lock().unwrap();
                        match r {
                            Ok(o) => {
                                changed |= s.orders.as_ref() != Some(&o) || s.orders_err.is_some();
                                s.orders = Some(o);
                                s.orders_err = None;
                            }
                            Err(e) => {
                                changed |= s.orders_err.as_deref() != Some(&e);
                                s.orders_err = Some(e);
                            }
                        }
                        next_orders = Instant::now() + ORDERS_EVERY;
                    }
                    sleep = sleep.min(next_orders.saturating_duration_since(Instant::now()));
                } else {
                    let mut s = st.lock().unwrap();
                    if s.orders.is_some() || s.orders_err.is_some() {
                        s.orders = None;
                        s.orders_err = None;
                        changed = true;
                    }
                }
                if changed {
                    unsafe {
                        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_NET, WPARAM(0), LPARAM(0));
                    }
                }
                match rx.recv_timeout(sleep.max(Duration::from_secs(1))) {
                    Ok(()) => {
                        let n = Instant::now();
                        next_print = n;
                        next_orders = n;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .ok();
    Handle { state, tx }
}

// -------------------------------------------------------------------- http --

fn q(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\r', '\n'], "")
}

/// GET `url` through curl.exe; `key` (if any) is sent as `x-nock-key`.
fn fetch(url: &str, key: &str) -> Result<String, String> {
    let mut cmd = Command::new("curl.exe");
    cmd.args(["-s", "-S", "-L", "--max-time", "8", "--connect-timeout", "4", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|_| "curl.exe not found".to_string())?;
    let mut conf = format!("url = \"{}\"\n", q(url));
    if !key.is_empty() {
        conf.push_str(&format!("header = \"x-nock-key: {}\"\n", q(key)));
    }
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(conf.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let e = err.lines().next().unwrap_or("").trim();
        Err(if e.is_empty() { "unreachable".into() } else { e.chars().take(60).collect() })
    }
}

// -------------------------------------------------------------------- json --

/// The raw text of the value for `"key":` in a flat JSON object.
fn raw<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{}\"", key);
    let i = body.find(&pat)? + pat.len();
    let rest = body[i..].trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with('"') {
        // string: up to the closing quote (skipping escaped ones)
        let b = rest.as_bytes();
        let mut j = 1;
        while j < b.len() {
            if b[j] == b'\\' {
                j += 2;
                continue;
            }
            if b[j] == b'"' {
                return Some(&rest[..=j]);
            }
            j += 1;
        }
        None
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(rest[..end].trim())
    }
}

fn unescape(s: &str) -> String {
    let s = s.trim_matches('"');
    let mut out = String::new();
    let mut it = s.chars().peekable();
    let mut units: Vec<u16> = Vec::new();
    let flush = |units: &mut Vec<u16>, out: &mut String| {
        if !units.is_empty() {
            out.push_str(&String::from_utf16_lossy(units));
            units.clear();
        }
    };
    while let Some(c) = it.next() {
        if c != '\\' {
            flush(&mut units, &mut out);
            out.push(c);
            continue;
        }
        match it.next() {
            Some('u') => {
                let hex: String = (0..4).filter_map(|_| it.next()).collect();
                if let Ok(u) = u16::from_str_radix(&hex, 16) {
                    units.push(u);
                }
            }
            Some(c2) => {
                flush(&mut units, &mut out);
                out.push(match c2 {
                    'n' | 'r' | 't' => ' ',
                    o => o,
                });
            }
            None => {}
        }
    }
    flush(&mut units, &mut out);
    out
}

fn text(body: &str, key: &str) -> String {
    match raw(body, key) {
        Some("null") | None => String::new(),
        Some(v) => unescape(v),
    }
}

fn num(body: &str, key: &str) -> f64 {
    raw(body, key).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
}

pub fn parse_print(body: &str) -> Result<Print, String> {
    if !body.trim_start().starts_with('{') {
        return Err("bad reply".into());
    }
    if let Some(e) = raw(body, "error") {
        return Err(unescape(e));
    }
    Ok(Print {
        state: text(body, "state"),
        progress: num(body, "progress").round() as i32,
        remaining_min: num(body, "remaining_min").round() as i32,
        job: text(body, "job"),
        nozzle: num(body, "nozzle_temp").round() as i32,
        bed: num(body, "bed_temp").round() as i32,
        layer: num(body, "layer") as i32,
        total_layers: num(body, "total_layers") as i32,
        needs_confirmation: raw(body, "needs_confirmation") == Some("true"),
    })
}

pub fn parse_orders(body: &str) -> Result<Orders, String> {
    if !body.trim_start().starts_with('{') {
        return Err("bad reply".into());
    }
    if let Some(e) = raw(body, "error") {
        return Err(unescape(e));
    }
    Ok(Orders {
        total: num(body, "total_orders") as i64,
        today: num(body, "today") as i64,
        latest_at: text(body, "latest_at"),
        latest_total: num(body, "latest_total"),
    })
}

/// Seconds since the Unix epoch for an ISO-8601 timestamp such as
/// `2026-10-16T09:30:00.123+00:00` (only UTC / `Z` / numeric offsets).
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = days * 86400 + h * 3600 + mi * 60 + se;
    // offset like +05:30 / -04:00 at the end
    let tail = &s[19..];
    if let Some(p) = tail.rfind(['+', '-']) {
        let off = &tail[p..];
        if off.len() >= 6 {
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let oh = off.get(1..3)?.parse::<i64>().ok()?;
            let om = off.get(4..6)?.parse::<i64>().ok()?;
            t -= sign * (oh * 3600 + om * 60);
        }
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_agent_feed() {
        let b = r#"{"state": "RUNNING", "progress": 42, "remaining_min": 37, "job": "Café stand.gcode", "layer": 10, "total_layers": 100, "bed_temp": 60.0, "nozzle_temp": 219.6, "needs_confirmation": false, "updated_at": 1.0}"#;
        let p = parse_print(b).unwrap();
        assert_eq!(p.state, "RUNNING");
        assert_eq!((p.progress, p.remaining_min, p.nozzle, p.bed), (42, 37, 220, 60));
        assert_eq!(p.job, "Caf\u{e9} stand.gcode");
        assert!(!p.needs_confirmation);
    }

    #[test]
    fn nulls_and_errors() {
        let p = parse_print(r#"{"state": null, "progress": null}"#).unwrap();
        assert_eq!(p.state, "");
        assert!(parse_print(r#"{"error": "nope"}"#).is_err());
        assert!(parse_orders("<html>").is_err());
    }

    #[test]
    fn parses_orders_and_time() {
        let o = parse_orders(r#"{"total_orders":12,"today":2,"latest_at":"2026-10-16T09:30:00.5+00:00","latest_total":499}"#).unwrap();
        assert_eq!((o.total, o.today), (12, 2));
        assert_eq!(parse_iso("1970-01-02T00:00:00Z"), Some(86400));
        assert_eq!(parse_iso(&o.latest_at), parse_iso("2026-10-16T15:00:00+05:30"));
    }
}
