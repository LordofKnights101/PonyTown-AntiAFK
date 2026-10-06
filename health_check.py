"""Health check for the running Pony Town Anti-AFK (either edition).

Read-only. Detects which edition is active and verifies, over that edition's
DevTools channel, that the game page is up and keep-alive activity is recent.
Prints one verdict line:

  HEALTHY: ...            everything nominal
  ISSUES: ...             something needs attention
  APP_CLOSED              neither edition is running
  ENGINE_DOWN: ...        app runs but its game engine is gone
  ENGINE_UNREACHABLE: ... engine up but DevTools not answering

Usage:  python health_check.py
"""

import json
import os
import re
import subprocess
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def run_ps(cmd):
    return subprocess.run(["powershell", "-NoProfile", "-Command", cmd],
                          capture_output=True, text=True).stdout.strip()


def detect_edition():
    """Returns (edition, port, extra_info) - edition is 'rust', 'python', or None."""
    out = subprocess.run(
        ["tasklist", "/fi", "imagename eq pony-antiafk.exe", "/fo", "csv"],
        capture_output=True, text=True).stdout
    if "pony-antiafk" in out:
        m = re.search(r"--remote-debugging-port=(\d+)", run_ps(
            "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" "
            "| Where-Object { $_.CommandLine -match 'remote-debugging-port' } "
            "| Select-Object -First 1 -ExpandProperty CommandLine"))
        if m:
            return "rust", int(m.group(1)), None
        return "rust", None, "app process is up but no embedded engine found"
    if run_ps("Get-CimInstance Win32_Process -Filter \"Name='pythonw.exe'\" "
              "| Where-Object { $_.CommandLine -match 'main\\.pyw' } "
              "| Select-Object -First 1 -ExpandProperty ProcessId"):
        m = re.search(r"--remote-debugging-port=(\d+)", run_ps(
            "Get-CimInstance Win32_Process "
            "-Filter \"Name='opera.exe' OR Name='msedge.exe' OR Name='chrome.exe'\" "
            "| Where-Object { $_.CommandLine -match 'pony AntiAfk\\\\profile' -and "
            "$_.CommandLine -match 'remote-debugging-port' } "
            "| Select-Object -First 1 -ExpandProperty CommandLine"))
        if m:
            return "python", int(m.group(1)), None
        return "python", None, "control panel is up but the game browser is not"
    return None, None, None


def get(url, timeout=5):
    return json.load(urllib.request.urlopen(url, timeout=timeout))


def main():
    edition, port, note = detect_edition()
    if edition is None:
        print("APP_CLOSED")
        return
    if port is None:
        print(f"ENGINE_DOWN: {note}")
        return

    try:
        ver = get(f"http://127.0.0.1:{port}/json/version")
        pages = [t for t in get(f"http://127.0.0.1:{port}/json/list")
                 if t.get("type") == "page"]
    except Exception as e:
        print(f"ENGINE_UNREACHABLE: {e}")
        return

    page_url = pages[0]["url"] if pages else None
    probe = {}
    if pages:
        try:
            sys.path.insert(0, os.path.join(HERE, "app"))
            import engine
            tab = engine._Tab(pages[0]["webSocketDebuggerUrl"])
            probe = tab.evaluate(
                "(function(){ var p = window.__ptaa; return {"
                " installed: !!p, last: p ? p.last : 0, now: Date.now(),"
                " canvas: !!(document.getElementById('canvas')||document.querySelector('canvas'))"
                " }; })()") or {}
        except Exception as e:
            probe = {"error": str(e)}

    away_s = max(0, (probe.get("now", 0) - probe.get("last", 0)) // 1000)
    issues = []
    if not pages:
        issues.append("no game page tab found")
    elif not page_url or "pony" not in page_url:
        issues.append(f"game page is not on pony.town ({page_url})")
    if not probe.get("installed"):
        issues.append("activity probe not installed (page still loading?)")
    elif probe.get("last") and away_s > 8 * 60:
        issues.append(f"no input activity for {away_s}s - keep-alives may have stopped")
    if probe.get("error"):
        issues.append(f"probe error: {probe['error']}")

    engine_name = ver.get("Browser", "?")
    base = f"[{edition} edition, engine {engine_name}]"
    if issues:
        print(f"ISSUES: {'; '.join(issues)} {base}"
              f" last activity {away_s}s ago")
    else:
        print(f"HEALTHY: page loaded (canvas={probe.get('canvas')}), "
              f"last keep-alive/user activity {away_s}s ago {base}")


if __name__ == "__main__":
    main()
