//! Minimal DevTools Protocol client - a Rust port of the Python app's
//! `app/engine.py` (hand-rolled WebSocket over localhost + CDP calls),
//! used against our own embedded WebView2 instance.
//!
//! Input dispatched through Input.dispatchKeyEvent / Input.dispatchMouseEvent
//! is trusted browser input (isTrusted == true), same property that made the
//! Python approach safe from clicker detection.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const KEY_CODES: &[(&str, u32)] = &[
    ("ArrowUp", 38),
    ("ArrowDown", 40),
    ("ArrowLeft", 37),
    ("ArrowRight", 39),
    ("w", 87),
    ("a", 65),
    ("s", 83),
    ("d", 68),
];

#[derive(Debug)]
pub enum CdpError {
    Io(std::io::Error),
    Closed,
    Timeout,
    Protocol(String),
    Json(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Io(e) => write!(f, "io error: {}", e),
            CdpError::Closed => write!(f, "DevTools connection lost"),
            CdpError::Timeout => write!(f, "DevTools call timed out"),
            CdpError::Protocol(s) => write!(f, "{}", s),
            CdpError::Json(s) => write!(f, "bad json: {}", s),
        }
    }
}

impl From<std::io::Error> for CdpError {
    fn from(e: std::io::Error) -> Self {
        CdpError::Io(e)
    }
}

pub fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(9223)
}

// ---------------------------------------------------------------------------
// Tiny HTTP client for /json/* endpoints (port of Browser._http)
// ---------------------------------------------------------------------------

fn http_json(port: u16, path: &str, method: &str) -> Result<Value, CdpError> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!(
        "{} /{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        method, path, port
    );
    stream.write_all(req.as_bytes())?;
    let body = read_http_body(&mut stream)?;
    let text = String::from_utf8_lossy(&body);
    serde_json::from_str(text.trim_start())
        .map_err(|e| CdpError::Json(e.to_string()))
}

/// Read one HTTP response body, honoring Content-Length / chunked encoding.
/// (DevTools keeps keep-alive connections open, so a naive read-to-end would
/// stall until the read timeout - the Python client avoided this via urllib.)
fn read_http_body(stream: &mut TcpStream) -> Result<Vec<u8>, CdpError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            return Err(CdpError::Protocol("connection closed before headers".into()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 65536 {
            return Err(CdpError::Protocol("http headers too large".into()));
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header terminator searched above");
    let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
    let mut body = buf[head_end + 4..].to_vec();

    if let Some(len) = header_value(&head, "content-length:") {
        let len: usize = len.trim().parse().unwrap_or(0);
        while body.len() < len {
            let n = stream.read(&mut byte)?;
            if n == 0 {
                break;
            }
            body.push(byte[0]);
        }
        body.truncate(len);
    } else if head.contains("transfer-encoding: chunked") {
        loop {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => body.push(byte[0]),
            }
        }
        let text = String::from_utf8_lossy(&body).to_string();
        body = dechunk(&text).into_bytes();
    } else {
        loop {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => body.push(byte[0]),
            }
        }
    }
    Ok(body)
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .find(|l| l.starts_with(name))
        .map(|l| l[name.len()..].to_string())
}

fn dechunk(body: &str) -> String {
    // Minimal chunked-transfer decoder: "size\r\n<bytes>\r\n" ... "0\r\n"
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some(line_end) = rest.find("\r\n") else { break };
        let size_str = rest[..line_end].split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_str, 16) else { break };
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        if rest.len() < start + size {
            out.push_str(&rest[start..]);
            break;
        }
        out.push_str(&rest[start..start + size]);
        rest = &rest[(start + size + 2).min(rest.len())..];
    }
    out
}

// ---------------------------------------------------------------------------
// Hand-rolled WebSocket client (port of engine.py `_WebSocket`)
// ---------------------------------------------------------------------------

struct Slot {
    response: Mutex<Option<Option<String>>>, // Some(None) = connection closed
    cv: Condvar,
}

struct WsShared {
    stream: Mutex<TcpStream>,
    alive: AtomicBool,
}

pub struct WsClient {
    shared: Arc<WsShared>,
    pending: Arc<Mutex<HashMap<u32, Arc<Slot>>>>,
    next_id: AtomicU32,
}

impl WsClient {
    pub fn connect(host: &str, port: u16, path: &str) -> Result<Self, CdpError> {
        let mut stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true)?;
        let key = {
            // 16 random-ish bytes; DevTools does not verify entropy.
            let mut b = [0u8; 16];
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut seed = nanos as u64 ^ 0x9E3779B97F4A7C15;
            for byte in b.iter_mut() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *byte = (seed >> 33) as u8;
            }
            base64(&b)
        };
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
            path, host, port, key
        );
        stream.write_all(req.as_bytes())?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut handshake = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte)?;
            if n == 0 {
                return Err(CdpError::Protocol("closed during handshake".into()));
            }
            handshake.extend_from_slice(&byte);
            if handshake.ends_with(b"\r\n\r\n") {
                break;
            }
            if handshake.len() > 65536 {
                return Err(CdpError::Protocol("handshake too large".into()));
            }
        }
        stream.set_read_timeout(None)?;
        let head = String::from_utf8_lossy(&handshake);
        let first = head.lines().next().unwrap_or("");
        if !first.contains(" 101 ") {
            return Err(CdpError::Protocol(format!("upgrade refused: {}", first)));
        }
        let lower = head.to_lowercase();
        if !lower.contains("upgrade: websocket") {
            return Err(CdpError::Protocol("not a websocket upgrade".into()));
        }

        let shared = Arc::new(WsShared {
            stream: Mutex::new(stream),
            alive: AtomicBool::new(true),
        });
        let client = WsClient {
            shared: shared.clone(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU32::new(1),
        };
        {
            // The reader gets its own duplicated socket handle so it never
            // contends with writers on the stream mutex (a long-held lock
            // here deadlocks every CDP call).
            let read_half = {
                let guard = shared.stream.lock().unwrap();
                guard.try_clone()?
            };
            let shared2 = shared.clone();
            let pending2 = client.pending.clone();
            std::thread::Builder::new()
                .name("ptaa-ws-reader".into())
                .spawn(move || reader_loop(read_half, shared2, pending2))?;
        }
        Ok(client)
    }

    pub fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    fn send_frame(&self, opcode: u8, payload: &[u8]) -> Result<(), CdpError> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let n = payload.len();
        if n < 126 {
            frame.push(0x80 | n as u8);
        } else if n < 65536 {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mask = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0x5EED);
            [(nanos >> 0) as u8, (nanos >> 8) as u8, (nanos >> 16) as u8, (nanos >> 24) as u8]
        };
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, c)| c ^ mask[i % 4]));
        let mut stream = self.shared.stream.lock().unwrap();
        stream.write_all(&frame)?;
        Ok(())
    }

    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let slot = Arc::new(Slot {
            response: Mutex::new(None),
            cv: Condvar::new(),
        });
        self.pending.lock().unwrap().insert(id, slot.clone());
        let payload = json!({"id": id, "method": method, "params": params});
        if let Err(e) = self.send_frame(0x1, payload.to_string().as_bytes()) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let guard = slot.response.lock().unwrap();
            match *guard {
                Some(Some(ref text)) => {
                    self.pending.lock().unwrap().remove(&id);
                    let msg: Value =
                        serde_json::from_str(text).map_err(|e| CdpError::Json(e.to_string()))?;
                    if let Some(err) = msg.get("error") {
                        return Err(CdpError::Protocol(format!(
                            "{} failed: {}",
                            method,
                            err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                        )));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                Some(None) => {
                    self.pending.lock().unwrap().remove(&id);
                    return Err(CdpError::Closed);
                }
                None => {
                    let mut guard = guard;
                    let until = deadline.min(Instant::now() + Duration::from_millis(200));
                    let wait = until.saturating_duration_since(Instant::now());
                    let (locked, _) = slot
                        .cv
                        .wait_timeout(guard, wait)
                        .expect("slot mutex poisoned");
                    if locked.is_some() {
                        continue;
                    }
                    if Instant::now() >= deadline {
                        self.pending.lock().unwrap().remove(&id);
                        if !self.alive() {
                            return Err(CdpError::Closed);
                        }
                        return Err(CdpError::Timeout);
                    }
                    guard = locked;
                    drop(guard);
                }
            }
        }
    }

    pub fn evaluate(&self, js: &str, timeout: Duration) -> Result<Value, CdpError> {
        let result = self.call(
            "Runtime.evaluate",
            json!({"expression": js, "returnByValue": true, "awaitPromise": false}),
            timeout,
        )?;
        if let Some(detail) = result.get("exceptionDetails") {
            let text = detail
                .pointer("/exception/description")
                .and_then(|v| v.as_str())
                .or_else(|| detail.get("text").and_then(|v| v.as_str()))
                .unwrap_or("?");
            return Err(CdpError::Protocol(format!("evaluate failed: {}", text)));
        }
        Ok(result.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    #[allow(dead_code)] // used by external CDP sessions and future shutdown paths
    pub fn close(&self) {
        let _ = self.send_frame(0x8, b"");
        self.shared.alive.store(false, Ordering::SeqCst);
    }
}

fn reader_loop(
    mut stream: TcpStream,
    shared: Arc<WsShared>,
    pending: Arc<Mutex<HashMap<u32, Arc<Slot>>>>,
) {
    fn read_exact(stream: &mut TcpStream, n: usize, buf: &mut Vec<u8>) -> bool {
        let start = buf.len();
        buf.resize(start + n, 0);
        stream.read_exact(&mut buf[start..]).is_ok()
    }

    let mut frag: Vec<u8> = Vec::new();
    loop {
        let mut header = Vec::new();
        if !read_exact(&mut stream, 2, &mut header) {
            break;
        }
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0F;
        let masked = header[1] & 0x80 != 0;
        let mut len = (header[1] & 0x7F) as u64;
        if len == 126 {
            let mut ext = Vec::new();
            if !read_exact(&mut stream, 2, &mut ext) {
                break;
            }
            len = u16::from_be_bytes([ext[0], ext[1]]) as u64;
        } else if len == 127 {
            let mut ext = Vec::new();
            if !read_exact(&mut stream, 8, &mut ext) {
                break;
            }
            len = u64::from_be_bytes([ext[0], ext[1], ext[2], ext[3], ext[4], ext[5], ext[6], ext[7]]);
        }
        let mut mask = Vec::new();
        if masked && !read_exact(&mut stream, 4, &mut mask) {
            break;
        }
        let mut data = Vec::new();
        if len > 0 && !read_exact(&mut stream, len as usize, &mut data) {
            break;
        }
        if masked {
            for (i, c) in data.iter_mut().enumerate() {
                *c ^= mask[i % 4];
            }
        }

        match opcode {
            0x9 => {
                // ping -> pong (client frames are always masked; the mask
                // bytes can be anything, so use a fixed one)
                let pong_mask = [0x12u8, 0x34, 0x56, 0x78];
                let mut f = vec![0x8A];
                f.push(0x80 | data.len() as u8);
                f.extend_from_slice(&pong_mask);
                f.extend(data.iter().enumerate().map(|(i, c)| c ^ pong_mask[i % 4]));
                let mut out = shared.stream.lock().unwrap();
                if out.write_all(&f).is_err() {
                    break;
                }
            }
            0x8 => break, // close
            0x1 | 0x2 | 0x0 => {
                let msg: Option<Vec<u8>> = if opcode != 0x0 && fin {
                    Some(data)
                } else if opcode != 0x0 {
                    frag = data;
                    None
                } else {
                    frag.extend_from_slice(&data);
                    if fin {
                        Some(std::mem::take(&mut frag))
                    } else {
                        None
                    }
                };
                if let Some(bytes) = msg {
                    let Ok(text) = String::from_utf8(bytes) else { continue };
                    let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                    let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else { continue };
                    if let Some(slot) = pending.lock().unwrap().get(&(id as u32)) {
                        let mut guard = slot.response.lock().unwrap();
                        *guard = Some(Some(text));
                        slot.cv.notify_all();
                    }
                }
            }
            _ => {}
        }
    }
    shared.alive.store(false, Ordering::SeqCst);
    if let Ok(map) = pending.lock() {
        for slot in map.values() {
            let mut guard = slot.response.lock().unwrap();
            if guard.is_none() {
                *guard = Some(None);
            }
            slot.cv.notify_all();
        }
    }
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// DevTools endpoint + game tab connection (port of Browser)
// ---------------------------------------------------------------------------

pub struct Devtools {
    pub port: u16,
}

pub struct Tab {
    ws: WsClient,
}

#[derive(Clone)]
pub struct Target {
    pub url: String,
    pub ws_url: String,
}

impl Devtools {
    pub fn new(port: u16) -> Self {
        Devtools { port }
    }

    pub fn is_up(&self) -> bool {
        http_json(self.port, "json/version", "GET").is_ok()
    }

    #[allow(dead_code)] // reserved: the app polls is_up() instead of blocking
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), CdpError> {
        let deadline = Instant::now() + timeout;
        loop {
            if http_json(self.port, "json/version", "GET").is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CdpError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    pub fn page_targets(&self) -> Vec<Target> {
        let Ok(list) = http_json(self.port, "json/list", "GET") else {
            return Vec::new();
        };
        list.as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        let is_page = t.get("type").and_then(|v| v.as_str()) == Some("page");
                        let ws = t
                            .get("webSocketDebuggerUrl")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let url = t.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if is_page {
                            ws.map(|ws_url| Target { url, ws_url })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn match_target(&self, domain: &str) -> Option<Target> {
        let targets = self.page_targets();
        targets
            .iter()
            .find(|t| t.url.contains(domain))
            .or_else(|| targets.first())
            .cloned()
    }
}

impl Tab {
    fn connect(target: &Target) -> Result<Tab, CdpError> {
        // ws://127.0.0.1:PORT/devtools/page/ID
        let rest = target
            .ws_url
            .strip_prefix("ws://")
            .ok_or_else(|| CdpError::Protocol("bad ws url".into()))?;
        let (hostport, path) = rest.split_once('/').ok_or_else(|| CdpError::Protocol("bad ws url".into()))?;
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(80)),
            None => (hostport.to_string(), 80),
        };
        let ws = WsClient::connect(&host, port, &format!("/{}", path))?;
        Ok(Tab { ws })
    }

    pub fn alive(&self) -> bool {
        self.ws.alive()
    }

    pub fn evaluate(&self, js: &str, timeout: Duration) -> Result<Value, CdpError> {
        self.ws.evaluate(js, timeout)
    }

    pub fn navigate(&self, url: &str) -> Result<(), CdpError> {
        self.ws
            .call("Page.navigate", json!({"url": url}), Duration::from_secs(10))
            .map(|_| ())
    }

    fn key(&self, key: &str, kind: &str) -> Result<(), CdpError> {
        let code = KEY_CODES
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, c)| *c)
            .unwrap_or_else(|| key.chars().next().map(|c| c.to_ascii_uppercase() as u32).unwrap_or(0));
        self.ws
            .call(
                "Input.dispatchKeyEvent",
                json!({
                    "type": kind,
                    "key": key,
                    "code": key,
                    "windowsVirtualKeyCode": code,
                    "nativeVirtualKeyCode": code,
                }),
                Duration::from_secs(5),
            )
            .map(|_| ())
    }

    pub fn key_tap(&self, key: &str, hold: Duration) -> Result<(), CdpError> {
        self.key(key, "rawKeyDown")?;
        std::thread::sleep(hold);
        self.key(key, "keyUp")
    }

    pub fn key_tap_pair(&self, k1: &str, k2: &str, hold: Duration, gap: Duration) -> Result<(), CdpError> {
        self.key_tap(k1, hold)?;
        std::thread::sleep(gap);
        self.key_tap(k2, hold)
    }

    pub fn mouse_move(&self, x: f64, y: f64) -> Result<(), CdpError> {
        self.ws
            .call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": x,
                    "y": y,
                    "button": "none",
                    "buttons": 0,
                    "pointerType": "mouse",
                }),
                Duration::from_secs(5),
            )
            .map(|_| ())
    }
}

pub struct GameConn {
    pub devtools: Devtools,
    pub domain: String,
    tab: Option<Tab>,
}

impl GameConn {
    pub fn new(port: u16, domain: String) -> Self {
        GameConn {
            devtools: Devtools::new(port),
            domain,
            tab: None,
        }
    }

    pub fn set_port(&mut self, port: u16) {
        self.devtools = Devtools::new(port);
        self.tab = None;
    }

    fn find_and_connect(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(target) = self.devtools.match_target(&self.domain) {
                if let Ok(tab) = Tab::connect(&target) {
                    self.tab = Some(tab);
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    /// Make sure we are attached to a page on the game domain.
    /// Returns Ok(true) = attached, Ok(false) = game tab not present yet.
    pub fn ensure_tab(&mut self) -> Result<bool, CdpError> {
        if let Some(tab) = &self.tab {
            if tab.alive() && tab.evaluate("1", Duration::from_secs(3)).is_ok() {
                return Ok(true);
            }
        }
        self.tab = None;
        if !self.devtools.is_up() {
            return Err(CdpError::Closed);
        }
        Ok(self.find_and_connect(Duration::from_secs(10)))
    }

    pub fn tab(&self) -> Option<&Tab> {
        self.tab.as_ref()
    }
}
