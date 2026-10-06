//! End-to-end test for the Rust port - mirrors `../tests/test_engine.py`.
//!
//! Drives the real cdp.rs + keepalive.rs engine against a mock page that
//! disconnects its "player" after 20 s without any TRUSTED input event:
//!   Phase A  step mode   -> mock never disconnects; events are trusted
//!   Phase B  protection off -> the mock DOES disconnect (control works)
//!   Phase C  browser exited cleanly

use pony_antiafk::cdp::{self, Devtools, WsClient};
use pony_antiafk::keepalive::{self, EngineParams, UiEvent};
use pony_antiafk::settings::Mode;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, TryRecvError};
use std::time::{Duration, Instant};

const MOCK_PAGE: &str = r#"
<!doctype html><html><head><meta charset="utf-8"><title>Mock Pony Town</title></head><body>
<canvas id="canvas" width="640" height="400" style="border:1px solid #000"></canvas>
<div id="status">connected</div>
<script>
window.__mock = { trusted: 0, untrusted: 0, lastTrusted: 0, disconnected: false };
function mark(e) {
  if (e.isTrusted) { window.__mock.trusted++; window.__mock.lastTrusted = Date.now(); }
  else { window.__mock.untrusted++; }
}
['keydown','keyup','mousedown','mousemove'].forEach(function (t) {
  window.addEventListener(t, mark, true);
});
var IDLE_MS = 20000, timer = null;
function disconnect() {
  window.__mock.disconnected = true;
  document.getElementById('status').textContent = 'DISCONNECTED (idle)';
  document.title = 'DISCONNECTED';
}
function arm() { clearTimeout(timer); timer = setTimeout(disconnect, IDLE_MS); }
['keydown','mousemove'].forEach(function (t) {
  window.addEventListener(t, function (e) { if (e.isTrusted) arm(); }, true);
});
arm();
</script></body></html>
"#;

fn check(cond: bool, label: &str) {
    if cond {
        println!("  PASS  {}", label);
    } else {
        panic!("TEST FAILED: {}", label);
    }
}

fn find_browser() -> Option<std::path::PathBuf> {
    let candidates = [
        r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe",
        r"%ProgramFiles%\Microsoft\Edge\Application\msedge.exe",
        r"%ProgramFiles%\Google\Chrome\Application\chrome.exe",
        r"%LocalAppData%\Programs\Opera GX\opera.exe",
    ];
    candidates
        .iter()
        .map(|c| std::path::PathBuf::from(dunce_path(c)))
        .find(|p| p.is_file())
}

fn dunce_path(var: &str) -> String {
    let expanded = if let Some(rest) = var.strip_prefix("%ProgramFiles(x86)%") {
        format!("C:\\Program Files (x86){}", rest)
    } else if let Some(rest) = var.strip_prefix("%ProgramFiles%") {
        format!("C:\\Program Files{}", rest)
    } else if let Some(rest) = var.strip_prefix("%LocalAppData%") {
        format!(
            "{}{}",
            std::env::var("LOCALAPPDATA").unwrap_or_default(),
            rest
        )
    } else {
        var.to_string()
    };
    expanded
}

/// Write the mock page to a temp file and return a file:// URL for it.
fn write_mock_page() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ptaa-rust-test");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("mock_idle_page.html");
    std::fs::write(&file, MOCK_PAGE).unwrap();
    file
}

fn launch_headless(url: &str, port: u16) -> (Child, std::path::PathBuf) {
    let profile = std::env::temp_dir().join(format!(
        "ptaa-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&profile).unwrap();
    let exe = find_browser().expect("no Chromium browser to test with");
    println!("Browser under test: {}", exe.display());
    let child = Command::new(&exe)
        .arg(format!("--remote-debugging-port={}", port))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--headless=new"])
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to launch browser");
    (child, profile)
}

fn kill_tree(child: &mut Child) {
    let pid = child.id();
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

/// Open a second CDP session to the game page and read window.__mock.
fn mock_state(devtools: &Devtools, domain: &str) -> serde_json::Value {
    let targets = devtools.page_targets();
    let target = targets
        .iter()
        .find(|t| domain.is_empty() || t.url.contains(domain))
        .expect("mock page target");
    let rest = target.ws_url.strip_prefix("ws://").unwrap();
    let (hostport, path) = rest.split_once('/').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    let ws = WsClient::connect(host, port.parse().unwrap(), &format!("/{}", path))
        .expect("second CDP session");
    let v = ws
        .evaluate("window.__mock", Duration::from_secs(10))
        .expect("evaluate __mock");
    ws.close();
    v
}

#[test]
fn engine_end_to_end() {
    let page = write_mock_page();
    let url = format!("file:///{}", page.display().to_string().replace('\\', "/"));
    let port = cdp::free_port();
    let (mut child, profile) = launch_headless(&url, port);

    let devtools = Devtools::new(port);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !devtools.is_up() {
        assert!(Instant::now() < deadline, "browser never exposed its debug port");
        if let Ok(Some(_)) = child.try_wait() {
            panic!("browser exited immediately");
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    // The engine matches tabs by URL substring; pin it to the mock page so we
    // never attach to a blank startup tab (headless Edge may create one).
    let domain = "mock_idle_page.html".to_string();

    // ---- Phase A: step-mode keep-alive keeps the mock alive ---------------
    println!("Phase A: step mode, keep-alive every 12 s, idle limit 20 s");
    let (tx, rx) = channel::<UiEvent>();
    let engine = keepalive::spawn(
        EngineParams {
            port,
            game_url: url.clone(),
            domain: domain.clone(),
            interval_sec: 12.0,
            mode: Mode::Step,
            skip_if_active: false,
        },
        tx,
    );

    // Let it run for 55 s (ticks land well inside the 20 s idle window).
    std::thread::sleep(Duration::from_secs(55));

    let mut logs = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(UiEvent::Log(l)) => logs.push(l),
            Ok(_) => {}
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => break,
        }
    }
    for l in &logs {
        println!("   | {}", l);
    }
    assert!(
        logs.iter().any(|l| l.contains("Keep-alive")),
        "no keep-alive log lines were produced"
    );

    let st = mock_state(&devtools, &domain);
    let trusted = st["trusted"].as_i64().unwrap_or(0);
    let untrusted = st["untrusted"].as_i64().unwrap_or(0);
    check(!st["disconnected"].as_bool().unwrap_or(true), "mock page never disconnected while protection ran");
    check(trusted >= 8, &format!("trusted key events arrived (n={})", trusted));
    check(untrusted == 0, "zero untrusted (synthetic) events");

    engine.stop();
    println!("  PASS  protection stopped cleanly");

    // ---- Phase B: without protection the mock must disconnect -------------
    println!("Phase B: protection off - mock should disconnect on its own");
    std::thread::sleep(Duration::from_secs(26));
    let st2 = mock_state(&devtools, &domain);
    check(st2["disconnected"].as_bool().unwrap_or(false), "mock disconnected without protection (control works)");

    // ---- Phase C: browser exits cleanly -----------------------------------
    kill_tree(&mut child);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().map(|o| o.is_none()).unwrap_or(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
    }
    check(child.try_wait().map(|o| o.is_some()).unwrap_or(false), "browser process exited cleanly");

    let _ = std::fs::remove_dir_all(&profile);
    println!("\nALL TESTS PASSED");
}
