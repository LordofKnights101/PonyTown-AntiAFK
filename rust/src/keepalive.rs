//! Keep-alive protection - Rust port of the Python app's `app/keepalive.py`.
//!
//! Every few minutes (randomized), sends a small trusted input event to the
//! embedded game view so Pony Town never counts you as idle:
//!   - "Tiny step": tap a movement key for ~50-90 ms, then the opposite key
//!     (the pony steps out and back to the same tile).
//!   - "Mouse wiggle": a few tiny trusted mouse moves over the canvas.
//! Skips when the user is actively playing and never types into a focused
//! chat box.

use crate::cdp::{CdpError, GameConn};
use crate::settings::Mode;
use crate::util;
use rand::Rng;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

pub const USER_INACTIVE_AFTER_MS: i64 = 120_000;
pub const OWN_ACTION_TOLERANCE_MS: i64 = 5_000;

pub const STEP_PAIRS: &[(&str, &str)] = &[
    ("ArrowLeft", "ArrowRight"),
    ("ArrowRight", "ArrowLeft"),
    ("ArrowUp", "ArrowDown"),
    ("ArrowDown", "ArrowUp"),
];

/// Installs the activity tracker at document-start and answers probes.
pub const PROBE_JS: &str = r#"
(function(){
  if(!window.__ptaa){
    window.__ptaa = {last: Date.now()};
    var mark = function(){ window.__ptaa.last = Date.now(); };
    ['keydown','pointerdown','mousedown','mousemove','wheel','touchstart']
      .forEach(function(t){ window.addEventListener(t, mark,
        {passive:true, capture:true}); });
  }
  var ae = document.activeElement;
  var c = document.getElementById('canvas') || document.querySelector('canvas');
  var rect = null;
  if (c) { var b = c.getBoundingClientRect();
           rect = {x: b.left, y: b.top, w: b.width, h: b.height}; }
  return { now: Date.now(),
           last: window.__ptaa.last,
           typing: !!(ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA'
                             || ae.isContentEditable)),
           canvas: !!c, rect: rect, url: location.href };
})()
"#;

#[derive(Debug, Clone)]
pub enum UiEvent {
    Log(String),
    Status(&'static str),
    WebViewDied,
}

pub enum EngineCmd {
    Stop,
    Restart { port: u16 },
}

pub struct EngineParams {
    pub port: u16,
    pub game_url: String,
    pub domain: String,
    pub interval_sec: f64,
    pub mode: Mode,
    pub skip_if_active: bool,
}

pub struct Engine {
    pub cmd_tx: Sender<EngineCmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    pub fn stop(mut self) {
        let _ = self.cmd_tx.send(EngineCmd::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn restart(&self, port: u16) {
        let _ = self.cmd_tx.send(EngineCmd::Restart { port });
    }
}

pub fn spawn(params: EngineParams, sink: Sender<UiEvent>) -> Engine {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<EngineCmd>();
    let handle = std::thread::Builder::new()
        .name("ptaa-engine".into())
        .spawn(move || run(params, cmd_rx, sink))
        .expect("spawn engine thread");
    Engine {
        cmd_tx,
        handle: Some(handle),
    }
}

fn log(sink: &Sender<UiEvent>, msg: String) {
    let _ = sink.send(UiEvent::Log(util::stamp(&msg)));
}

fn status(sink: &Sender<UiEvent>, s: &'static str) {
    let _ = sink.send(UiEvent::Status(s));
}

fn run(params: EngineParams, cmds: Receiver<EngineCmd>, sink: Sender<UiEvent>) {
    let mut p = params;
    let mut conn = GameConn::new(p.port, p.domain.clone());
    let mut first = true;
    let mut errors: u32 = 0;
    let mut own_last: Option<i64> = None;

    status(&sink, "starting");
    let _ = sink.send(UiEvent::Log(util::stamp(&format!(
        "Engine started (devtools port {})",
        p.port
    ))));

    loop {
        let wait = if first {
            first = false;
            Duration::from_secs(10)
        } else {
            jittered(p.interval_sec)
        };

        match cmds.recv_timeout(wait) {
            Ok(EngineCmd::Stop) => break,
            Ok(EngineCmd::Restart { port }) => {
                conn.set_port(port);
                p.port = port;
                errors = 0;
                status(&sink, "starting");
                log(&sink, format!("Game engine re-attached (port {})", port));
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        match tick(&p, &mut conn, own_last, &sink) {
            TickResult::Sent(own) => {
                errors = 0;
                if own {
                    own_last = Some(now_ms() + 1000);
                }
            }
            TickResult::Died => {
                let _ = sink.send(UiEvent::WebViewDied);
                // The UI recreates the web view and sends Restart; wait for it.
                match cmds.recv_timeout(Duration::from_secs(60)) {
                    Ok(EngineCmd::Restart { port }) => {
                        conn.set_port(port);
                        p.port = port;
                        errors = 0;
                        status(&sink, "starting");
                        log(&sink, format!("Game engine re-attached (port {})", port));
                    }
                    _ => break,
                }
            }
            TickResult::Soft(e) => {
                errors += 1;
                log(&sink, format!("Connection hiccup ({})", e));
                status(&sink, "waiting");
                if errors >= 10 {
                    log(&sink, "Too many failures - giving up. Press Start to try again.".into());
                    status(&sink, "error");
                    return;
                }
                recover(&mut conn, &sink);
            }
        }
    }
    status(&sink, "off");
}

fn recover(conn: &mut GameConn, sink: &Sender<UiEvent>) {
    if !conn.devtools.is_up() {
        // The web view process itself is gone; the next tick reports
        // WebViewDied and the UI rebuilds it.
        return;
    }
    if !conn.ensure_tab().unwrap_or(false) {
        log(sink, "Game tab not found yet - will keep retrying.".into());
    }
}

enum TickResult {
    /// A keep-alive was sent (so `own_last` should be updated).
    Sent(bool),
    /// Transient problem - retry with backoff.
    Soft(String),
    /// The embedded browser process died - the UI must recreate the view.
    Died,
}

fn tick(p: &EngineParams, conn: &mut GameConn, own_last: Option<i64>, sink: &Sender<UiEvent>) -> TickResult {
    match conn.ensure_tab() {
        Ok(true) => {}
        Ok(false) => {
            log(sink, format!("Game tab not found - open {} in the window.", p.game_url));
            status(sink, "waiting");
            return TickResult::Sent(false);
        }
        Err(e) => {
            log(sink, format!("Game engine unreachable ({}) - requesting restart.", e));
            return TickResult::Died;
        }
    }
    status(sink, "running");

    let tab = conn.tab().expect("tab attached");
    let probe = match tab.evaluate(PROBE_JS, Duration::from_secs(10)) {
        Ok(v) => v,
        Err(e) => return TickResult::Soft(e.to_string()),
    };
    let Some(obj) = probe.as_object() else {
        return TickResult::Soft("game page not responding".into());
    };

    let now = obj.get("now").and_then(|v| v.as_i64()).unwrap_or(0);
    let last = obj.get("last").and_then(|v| v.as_i64()).unwrap_or(0);
    let typing = obj.get("typing").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_canvas = obj.get("canvas").and_then(|v| v.as_bool()).unwrap_or(false);
    let rect = obj.get("rect").cloned().unwrap_or(serde_json::Value::Null);
    let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();

    if !p.domain.is_empty() && !url.contains(&p.domain) {
        log(sink, format!("Game tab is on another page ({}) - going back to {} ...", url, p.game_url));
        let _ = tab.navigate(&p.game_url);
        return TickResult::Sent(false);
    }

    // Skip if the user is genuinely active.
    if p.skip_if_active {
        let away_for = now - last;
        let looks_own = own_last
            .map(|o| (last - o).abs() < OWN_ACTION_TOLERANCE_MS)
            .unwrap_or(false);
        if away_for < USER_INACTIVE_AFTER_MS && !looks_own {
            log(sink, "You're actively playing - keep-alive skipped.".into());
            return TickResult::Sent(false);
        }
    }

    let mut mode = p.mode;
    if mode == Mode::Step && (typing || !has_canvas) {
        let reason = if typing { "chat box is focused" } else { "game canvas not found" };
        log(sink, format!("{} - using a mouse wiggle instead this time.", reason));
        mode = Mode::Wiggle;
    } else if mode == Mode::Both {
        mode = if rand::thread_rng().gen_bool(0.5) { Mode::Step } else { Mode::Wiggle };
    }

    match mode {
        Mode::Step => {
            let (k1, k2) = STEP_PAIRS[rand::thread_rng().gen_range(0..STEP_PAIRS.len())];
            let hold = Duration::from_millis(rand::thread_rng().gen_range(50..=90));
            let gap = Duration::from_millis(rand::thread_rng().gen_range(150..=250));
            if let Err(e) = tab.key_tap_pair(k1, k2, hold, gap) {
                return TickResult::Soft(e.to_string());
            }
            log(sink, format!("Keep-alive: tiny step ({} then {})", k1, k2));
        }
        _ => {
            if let Err(e) = wiggle(tab, &rect) {
                return TickResult::Soft(e.to_string());
            }
            log(sink, "Keep-alive: mouse wiggle".into());
        }
    }
    TickResult::Sent(true)
}

fn wiggle(tab: &crate::cdp::Tab, rect: &serde_json::Value) -> Result<(), CdpError> {
    let (cx, cy) = if let Some(r) = rect.as_object() {
        let x = r.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = r.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let w = r.get("w").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let h = r.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if w > 0.0 {
            (x + w * 0.5 + jitter(40.0), y + h * 0.5 + jitter(30.0))
        } else {
            (40.0, 40.0)
        }
    } else {
        (40.0, 40.0)
    };
    for (dx, dy) in [(-3.0, 0.0), (3.0, 1.0), (-1.0, -2.0)] {
        tab.mouse_move(cx + dx, cy + dy)?;
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn jitter(range: f64) -> f64 {
    rand::thread_rng().gen_range(-range..=range)
}

fn jittered(interval_sec: f64) -> Duration {
    Duration::from_secs_f64(interval_sec * rand::thread_rng().gen_range(0.75..=1.25))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
