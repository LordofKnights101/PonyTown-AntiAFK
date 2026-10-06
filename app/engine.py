"""Browser engine for Pony Town Anti-AFK.

Launches a Chromium browser (Edge or Chrome) with a dedicated profile and
drives it over the Chrome DevTools Protocol using a tiny hand-rolled
WebSocket client, so the app has zero third-party dependencies.

Input sent through Input.dispatchKeyEvent / Input.dispatchMouseEvent is
real browser-level input (isTrusted == true), indistinguishable from a
human pressing keys -- unlike page-level synthetic events, which Pony Town
can detect (and flags accounts for).
"""

import base64
import json
import os
import socket
import struct
import subprocess
import threading
import time
import urllib.parse
import urllib.request

IS_WINDOWS = os.name == "nt"


class ProtocolError(Exception):
    """A DevTools call failed or returned an error."""


class WebSocketClosed(Exception):
    """The DevTools WebSocket connection was lost."""


def _free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# ---------------------------------------------------------------------------
# Minimal WebSocket client (client side of RFC 6455, text frames only)
# ---------------------------------------------------------------------------

class _WebSocket:
    def __init__(self, url):
        u = urllib.parse.urlparse(url)
        self._host = u.hostname
        self._port = u.port or 80
        self._path = u.path or "/"
        if u.query:
            self._path += "?" + u.query
        self.sock = None
        self._alive = False
        self._send_lock = threading.Lock()
        self.on_text = None      # callback(str), called from reader thread
        self.on_close = None     # callback(), called from reader thread

    @property
    def alive(self):
        return self._alive

    def connect(self, timeout=10):
        self.sock = socket.create_connection((self._host, self._port), timeout=timeout)
        self.sock.settimeout(None)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            "GET %s HTTP/1.1\r\n"
            "Host: %s:%d\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            "Sec-WebSocket-Key: %s\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n" % (self._path, self._host, self._port, key)
        )
        self.sock.sendall(req.encode())
        # Read the handshake response byte-by-byte until end of headers.
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(1)
            if not chunk:
                raise ProtocolError("connection closed during handshake")
            buf += chunk
            if len(buf) > 65536:
                raise ProtocolError("handshake response too large")
        head = buf.split(b"\r\n\r\n", 1)[0].decode("latin-1")
        lines = head.split("\r\n")
        if " 101 " not in lines[0]:
            raise ProtocolError("websocket upgrade refused: " + lines[0])
        headers = {k.strip().lower(): v.strip() for k, v in
                   (l.split(":", 1) for l in lines[1:] if ":" in l)}
        if "websocket" not in headers.get("upgrade", "").lower():
            raise ProtocolError("not a websocket upgrade response")
        self._alive = True
        t = threading.Thread(target=self._reader, daemon=True, name="ptaa-ws-reader")
        t.start()

    def _read_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("socket closed")
            buf += chunk
        return buf

    def _send_frame(self, opcode, payload: bytes):
        frame = bytearray()
        frame.append(0x80 | opcode)
        n = len(payload)
        if n < 126:
            frame.append(0x80 | n)
        elif n < 65536:
            frame.append(0x80 | 126)
            frame += struct.pack(">H", n)
        else:
            frame.append(0x80 | 127)
            frame += struct.pack(">Q", n)
        mask = os.urandom(4)
        frame += mask
        frame += bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        with self._send_lock:
            self.sock.sendall(bytes(frame))

    def send_text(self, text):
        if not self._alive:
            raise WebSocketClosed("connection is closed")
        self._send_frame(0x1, text.encode("utf-8"))

    def close(self):
        if self._alive:
            try:
                self._send_frame(0x8, b"")
            except Exception:
                pass
        self._alive = False
        try:
            self.sock.close()
        except Exception:
            pass

    def _reader(self):
        frag_buf = bytearray()
        try:
            while self._alive:
                h = self._read_exact(2)
                fin = bool(h[0] & 0x80)
                opcode = h[0] & 0x0F
                masked = bool(h[1] & 0x80)
                n = h[1] & 0x7F
                if n == 126:
                    n = struct.unpack(">H", self._read_exact(2))[0]
                elif n == 127:
                    n = struct.unpack(">Q", self._read_exact(8))[0]
                mask = self._read_exact(4) if masked else None
                data = self._read_exact(n) if n else b""
                if mask:
                    data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))

                if opcode == 0x9:            # ping -> pong
                    self._send_frame(0xA, data)
                    continue
                if opcode == 0x8:            # close
                    break
                if opcode in (0x1, 0x2, 0x0):
                    if opcode != 0x0 and fin:
                        msg = data
                    elif opcode != 0x0:      # start of fragmented message
                        frag_buf = bytearray(data)
                        continue
                    else:                    # continuation
                        frag_buf += data
                        if not fin:
                            continue
                        msg = bytes(frag_buf)
                    if self.on_text:
                        try:
                            self.on_text(msg.decode("utf-8"))
                        except Exception:
                            pass
                # Binary frames from DevTools never occur for our usage;
                # ignore anything else.
        except Exception:
            pass
        finally:
            self._alive = False
            try:
                self.sock.close()
            except Exception:
                pass
            if self.on_close:
                try:
                    self.on_close()
                except Exception:
                    pass


# ---------------------------------------------------------------------------
# DevTools tab connection
# ---------------------------------------------------------------------------

class _Tab:
    """A CDP session attached to one page target."""

    def __init__(self, ws_url):
        self._ws = _WebSocket(ws_url)
        self._pending = {}
        self._id_lock = threading.Lock()
        self._next_id = 0
        self._ws.on_text = self._on_message
        self._ws.on_close = self._on_closed
        self._ws.connect()
        self.lock = threading.Lock()

    def _on_closed(self):
        for ev, _slot in list(self._pending.values()):
            ev.set()

    def _on_message(self, text):
        try:
            msg = json.loads(text)
        except ValueError:
            return
        mid = msg.get("id")
        if mid is not None:
            slot = self._pending.get(mid)
            if slot is not None:
                slot[1] = msg
                slot[0].set()
        # Events (no id) are ignored; nothing needs them right now.

    def call(self, method, timeout=10, **params):
        with self._id_lock:
            self._next_id += 1
            mid = self._next_id
        payload = json.dumps({"id": mid, "method": method, "params": params})
        slot = [threading.Event(), None]
        with self.lock:
            self._pending[mid] = slot
        try:
            self._ws.send_text(payload)
        except WebSocketClosed:
            self._pending.pop(mid, None)
            raise
        deadline = time.time() + timeout
        while not slot[0].wait(0.2):
            if time.time() > deadline:
                self._pending.pop(mid, None)
                raise ProtocolError("%s timed out" % method)
            if not self._ws.alive:
                self._pending.pop(mid, None)
                raise WebSocketClosed("DevTools connection lost")
        self._pending.pop(mid, None)
        if slot[1] is None:
            raise WebSocketClosed("no response from DevTools")
        resp = slot[1]
        if "error" in resp:
            raise ProtocolError("%s failed: %s" % (method, resp["error"].get("message")))
        return resp.get("result", {})

    def evaluate(self, expression, timeout=10):
        r = self.call("Runtime.evaluate", timeout, expression=expression,
                      returnByValue=True, awaitPromise=False)
        if "exceptionDetails" in r:
            detail = r["exceptionDetails"]
            text = detail.get("exception", {}).get("description") or detail.get("text", "?")
            raise ProtocolError("evaluate failed: %s" % text)
        return r.get("result", {}).get("value")

    def close(self):
        self._ws.close()


# ---------------------------------------------------------------------------
# Browser control
# ---------------------------------------------------------------------------

_BROWSER_CANDIDATES = [
    # Opera GX first - the browser the user actually plays with.
    r"%LocalAppData%\Programs\Opera GX\opera.exe",
    r"%ProgramFiles%\Opera GX\opera.exe",
    r"%ProgramFiles(x86)%\Opera GX\opera.exe",
    r"%LocalAppData%\Programs\Opera\opera.exe",
    r"%ProgramFiles%\Opera\opera.exe",
    r"%ProgramFiles%\Google\Chrome\Application\chrome.exe",
    r"%ProgramFiles(x86)%\Google\Chrome\Application\chrome.exe",
    r"%LocalAppData%\Google\Chrome\Application\chrome.exe",
    r"%ProgramFiles%\Microsoft\Edge\Application\msedge.exe",
    r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe",
]


def find_browser(preferred=None):
    """Return a path to a Chromium browser (Opera GX / Opera / Chrome /
    Edge), or None."""
    if preferred and os.path.isfile(preferred):
        return preferred
    for cand in _BROWSER_CANDIDATES:
        path = os.path.expandvars(cand)
        if os.path.isfile(path):
            return path
    if IS_WINDOWS:
        try:
            import winreg
            for hive in (winreg.HKEY_LOCAL_MACHINE, winreg.HKEY_CURRENT_USER):
                for name in ("opera.exe", "chrome.exe", "msedge.exe"):
                    try:
                        key = winreg.OpenKey(
                            hive,
                            r"SOFTWARE\Microsoft\Windows\CurrentVersion"
                            r"\App Paths\\" + name)
                        val = winreg.QueryValueEx(key, None)[0]
                        winreg.CloseKey(key)
                        val = val.strip('"')
                        if os.path.isfile(val):
                            return val
                    except OSError:
                        continue
        except Exception:
            pass
    return None


class Browser:
    """A Chromium browser instance with one game tab, driven over CDP."""

    def __init__(self, exe, profile_dir, game_url, headless=False, log=print):
        self.exe = exe
        self.profile_dir = profile_dir
        self.game_url = game_url
        self.headless = headless
        self.log = log
        self._proc = None
        self._port = None
        self._tab = None
        self.domain = urllib.parse.urlparse(game_url).netloc

    # -- lifecycle ----------------------------------------------------------

    @property
    def running(self):
        return self._proc is not None and self._proc.poll() is None

    def is_up(self):
        """True if the debug port answers - the browser is actually usable,
        even if the launcher process we spawned has handed off and exited."""
        try:
            self._http("json/version", timeout=2)
            return True
        except Exception:
            return False

    @property
    def connected(self):
        return self._tab is not None and self._tab._ws.alive

    def launch(self, timeout=30):
        """Start the browser process and wait for a game tab to attach to."""
        if self.running:
            raise RuntimeError("browser already running")
        os.makedirs(self.profile_dir, exist_ok=True)
        self._port = _free_port()
        args = [
            self.exe,
            "--remote-debugging-port=%d" % self._port,
            "--user-data-dir=" + self.profile_dir,
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-sync",
            "--hide-crash-restore-bubble",
        ]
        if self.headless:
            args.append("--headless=new")
        args += ["--new-window", self.game_url]
        self._proc = subprocess.Popen(
            args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # Poll for the debug port. Some browsers hand off through a launcher
        # stub that exits immediately - that's fine as long as the port
        # comes up, so only fail if it never does.
        deadline = time.time() + timeout
        while True:
            try:
                self._http("json/version", timeout=2)
                break
            except Exception:
                if time.time() >= deadline:
                    raise RuntimeError(
                        "browser never exposed its debug port "
                        "(process %s)" % ("exited" if not self.running
                                          else "still running"))
                time.sleep(0.3)
        if not self._find_and_connect_tab(timeout):
            self.log("Game tab not open yet - waiting for it to load...")

    def shutdown(self, kill=False):
        if self._tab:
            try:
                self._tab.close()
            except Exception:
                pass
            self._tab = None
        if kill:
            try:
                self._proc.terminate()
            except Exception:
                pass
            try:
                self._proc.wait(timeout=8)
            except Exception:
                pass
            # Belt and braces: end anything still bound to OUR profile dir
            # only - never the user's own browser windows.
            self._kill_by_profile()
            self._proc = None

    def _kill_by_profile(self):
        if not IS_WINDOWS:
            return
        pat = self.profile_dir.replace("'", "''")
        try:
            subprocess.run(
                ["powershell", "-NoProfile", "-Command",
                 "Get-CimInstance Win32_Process -Filter \"Name='opera.exe' "
                 "OR Name='launcher.exe' OR Name='chrome.exe' OR "
                 "Name='msedge.exe'\" | Where-Object { $_.CommandLine -ne "
                 "$null -and $_.CommandLine.Contains('%s') } | "
                 "ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"
                 % pat],
                capture_output=True, timeout=30)
        except Exception:
            pass

    def _http(self, path, timeout=3, method=None):
        url = "http://127.0.0.1:%d/%s" % (self._port, path)
        req = urllib.request.Request(url, method=method) if method else urllib.request.Request(url)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode("utf-8"))

    # -- tab management -----------------------------------------------------

    def _page_targets(self):
        try:
            targets = self._http("json/list")
        except Exception:
            return []
        return [t for t in targets
                if t.get("type") == "page" and t.get("webSocketDebuggerUrl")]

    def _match_target(self, targets):
        for t in targets:
            if self.domain and self.domain in (t.get("url") or ""):
                return t
        return None

    def _find_and_connect_tab(self, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            target = self._match_target(self._page_targets())
            if target:
                try:
                    self._tab = _Tab(target["webSocketDebuggerUrl"])
                    url = target.get("url", "")
                    self.log("Connected to game tab: %s" % (url or "(loading)"))
                    return True
                except Exception:
                    pass
            time.sleep(0.5)
        return False

    def _open_tab(self):
        q = urllib.parse.quote(self.game_url, safe="")
        try:
            self._http("json/new?url=" + q, method="PUT")
        except Exception:
            try:
                self._http("json/new?" + q)  # older Chromium used GET
            except Exception:
                pass

    def ensure_tab(self, timeout=20):
        """Make sure we are attached to a page on the game domain."""
        if self.connected:
            try:
                self._tab.evaluate("1", timeout=3)
                return True
            except Exception:
                try:
                    self._tab.close()
                except Exception:
                    pass
                self._tab = None
        if not self.is_up():
            raise RuntimeError("browser process is gone")
        if not self._find_and_connect_tab(timeout=timeout / 2):
            self._open_tab()
            if not self._find_and_connect_tab(timeout=timeout / 2):
                return False
        return True

    # -- actions -------------------------------------------------------------

    def evaluate(self, js, timeout=10):
        if not self.connected:
            raise WebSocketClosed("no tab attached")
        return self._tab.evaluate(js, timeout)

    def bring_to_front(self):
        if self.connected:
            self._tab.call("Page.bringToFront", 5)

    def navigate(self, url):
        if self.connected:
            self._tab.call("Page.navigate", 10, url=url)

    _KEY_CODES = {
        "ArrowUp": 38, "ArrowDown": 40, "ArrowLeft": 37, "ArrowRight": 39,
        "w": 87, "a": 65, "s": 83, "d": 68,
    }

    def _key(self, key, type_):
        code = self._KEY_CODES.get(key)
        if code is None:
            code = ord(key[:1].upper())
        self._tab.call("Input.dispatchKeyEvent", 5, type=type_, key=key, code=key,
                       windowsVirtualKeyCode=code, nativeVirtualKeyCode=code)

    def key_tap(self, key, hold_s):
        """Press and release one key."""
        self._key(key, "rawKeyDown")
        time.sleep(hold_s)
        self._key(key, "keyUp")

    def key_tap_pair(self, k1, k2, hold_s, gap_s):
        """Tap one key, then the opposite one - the pony steps out and back."""
        self.key_tap(k1, hold_s)
        time.sleep(gap_s)
        self.key_tap(k2, hold_s)

    def canvas_rect(self):
        try:
            return self.evaluate(
                "(function(){var c=document.getElementById('canvas')||"
                "document.querySelector('canvas'); if(!c) return null;"
                "var b=c.getBoundingClientRect();"
                "return {x:b.left,y:b.top,w:b.width,h:b.height};})()")
        except Exception:
            return None

    def mouse_wiggle(self):
        """A few tiny trusted mouse moves over the game canvas."""
        rect = self.canvas_rect()
        if rect and rect.get("w"):
            cx = rect["x"] + rect["w"] * 0.5 + (time.time_ns() % 80 - 40)
            cy = rect["y"] + rect["h"] * 0.5 + (time.time_ns() % 60 - 30)
        else:
            cx, cy = 40.0, 40.0
        for dx, dy in ((-3, 0), (3, 1), (-1, -2)):
            self._tab.call("Input.dispatchMouseEvent", 5, type="mouseMoved",
                           x=cx + dx, y=cy + dy, button="none", buttons=0,
                           pointerType="mouse")
            time.sleep(0.05)
