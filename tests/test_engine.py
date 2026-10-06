"""End-to-end test for Pony Town Anti-AFK.

Runs a local mock page that disconnects its "player" after 20 s without any
TRUSTED input event (the same class of event a real pony.town session reacts
to), then verifies:

  Phase A  step mode      -> mock never disconnects; events are trusted
  Phase B  wiggle mode    -> trusted mouse events arrive
  Phase C  with no engine -> the mock DOES disconnect (proves the test can fail)

Run:  python tests/test_engine.py
"""

import functools
import http.server
import os
import socketserver
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = os.path.join(os.path.dirname(HERE), "app")
sys.path.insert(0, APP)

from engine import find_browser          # noqa: E402
from keepalive import Protection         # noqa: E402

CHECK_SECONDS = 55
NEGATIVE_WAIT = 26


class Fail(Exception):
    pass


def check(cond, label):
    if cond:
        print("  PASS  %s" % label)
    else:
        raise Fail(label)


def serve_tests():
    handler = functools.partial(http.server.SimpleHTTPRequestHandler,
                                directory=HERE)
    handler.log_message = lambda *a, **k: None
    srv = socketserver.ThreadingTCPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    port = srv.server_address[1]
    return srv, "http://127.0.0.1:%d/mock_idle_page.html" % port


def wait_until(fn, timeout, what):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if fn():
                return
        except Exception:
            pass
        time.sleep(0.5)
    raise Fail("timed out waiting for: " + what)


def make_protection(url, profile, mode, interval_sec, logs):
    def log(msg):
        logs.append(msg)
        print("   | %s" % msg)
    return Protection(profile_dir=profile, game_url=url,
                      interval_sec=interval_sec, mode=mode,
                      skip_if_active=False, auto_reopen=False,
                      headless=HEADLESS, log=log,
                      status=lambda s: print("   [status] %s" % s))


def mock_state(browser):
    return browser.evaluate("window.__mock")


def kill_leftovers():
    """Kill only browser processes started by a previous test run (their
    command line mentions our temp profile prefix). Never touches the
    user's own browser windows."""
    try:
        subprocess.run(
            ["powershell", "-NoProfile", "-Command",
             "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' OR "
             "Name='chrome.exe' OR Name='opera.exe' OR Name='launcher.exe'\" "
             "| Where-Object { $_.CommandLine -ne $null -and "
             "$_.CommandLine.Contains('ptaa-test') } | ForEach-Object { "
             "Stop-Process -Id $_.ProcessId -Force }"],
            capture_output=True, timeout=30)
    except Exception:
        pass


HEADLESS = "--headed" not in sys.argv


def main():
    kill_leftovers()
    exe = find_browser()
    if not exe:
        raise Fail("no Chromium browser found to test with")
    print("Browser under test: %s (headless=%s)" % (exe, HEADLESS))
    srv, url = serve_tests()
    print("Mock game page: %s\n" % url)

    profile = tempfile.mkdtemp(prefix="ptaa-test-")

    # ---- Phase A: step mode keeps the mock alive -------------------------
    print("Phase A: step mode, keep-alive every 12 s, idle limit 20 s")
    logs = []
    prot = make_protection(url, profile, "step", 12, logs)
    prot.start()
    wait_until(lambda: prot.browser is not None and prot.browser.connected,
               45, "browser attached to mock page")
    for i in range(CHECK_SECONDS // 5):
        time.sleep(5)
        st = mock_state(prot.browser)
        if st["disconnected"]:
            raise Fail("mock disconnected during protection (t=%ds)" % ((i + 1) * 5))
    st = mock_state(prot.browser)
    check(not st["disconnected"], "mock page never disconnected while protection ran")
    check(st["trusted"] >= 8, "trusted key events arrived (n=%d)" % st["trusted"])
    check(st["untrusted"] == 0, "zero untrusted (synthetic) events")
    prot.stop()
    prot.join(timeout=15)
    check(not prot.running, "protection stopped cleanly")
    prot.browser.shutdown(kill=True)   # free the profile for phase B
    wait_until(lambda: not prot.browser.running, 20, "phase A browser exited")

    # ---- Phase B: wiggle mode --------------------------------------------
    print("\nPhase B: wiggle mode on a fresh browser")
    prot2 = make_protection(url, profile, "wiggle", 10, [])
    prot2.start()
    wait_until(lambda: prot2.browser is not None and prot2.browser.connected,
               45, "second browser attached")
    time.sleep(26)
    st2 = mock_state(prot2.browser)
    check(not st2["disconnected"], "mock still connected during wiggle mode")
    check(st2["trusted"] >= 3,
          "trusted mouse events arrived (n=%d)" % st2["trusted"])
    prot2.stop()
    prot2.join(timeout=15)

    # ---- Phase C: negative control - no protection -----------------------
    print("\nPhase C: protection off - mock should disconnect on its own")
    live_browser = prot2.browser
    time.sleep(NEGATIVE_WAIT)
    st3 = mock_state(live_browser)
    check(st3["disconnected"], "mock disconnected without protection (control works)")

    live_browser.shutdown(kill=True)
    wait_until(lambda: not live_browser.running, 20, "browser process exited")
    check(True, "browser process exited cleanly")
    srv.shutdown()

    print("\nALL TESTS PASSED")


if __name__ == "__main__":
    try:
        main()
    except Fail as e:
        print("\nTEST FAILED: %s" % e)
        sys.exit(1)
