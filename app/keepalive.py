"""Keep-alive protection for Pony Town.

Runs the browser and periodically sends small, trusted input events so the
game never counts you as idle:

  - "Tiny step" mode taps a movement key for a few milliseconds and then the
    opposite key, so the pony turns/steps out and back to the same tile.
    This is exactly what real players do constantly, and because the events
    go through the browser's DevTools input pipeline they are trusted user
    input (isTrusted == true), not page-level synthetic events.
  - "Mouse wiggle" mode makes a few tiny mouse moves over the canvas.

Every few minutes, with random jitter, and only when:
  - you have not interacted yourself recently (your real input is tracked),
  - the chat box is not focused (so nothing gets typed into chat).

The game window never needs focus or to be visible; it can sit in the
background the whole time.
"""

import os
import random
import threading
import time

from engine import Browser, find_browser

# How long the user must have been inactive (ms) before we act.
USER_INACTIVE_AFTER_MS = 120_000
# If the recorded last-activity matches our own action this closely (ms),
# it was our keep-alive, not the user.
OWN_ACTION_TOLERANCE_MS = 5_000

STEP_PAIRS = [
    ("ArrowLeft", "ArrowRight"),
    ("ArrowRight", "ArrowLeft"),
    ("ArrowUp", "ArrowDown"),
    ("ArrowDown", "ArrowUp"),
]

PROBE_JS = """
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
"""

MODE_LABELS = {
    "step": "Tiny step (recommended)",
    "wiggle": "Mouse wiggle only",
    "both": "Both (random)",
}


class Protection:
    """Owns the browser and the keep-alive loop. Safe to start/stop repeatedly."""

    def __init__(self, *, browser_path=None, profile_dir, game_url,
                 interval_sec=240.0, mode="step", skip_if_active=True,
                 auto_reopen=True, headless=False, log=print, status=None):
        self.browser_path = browser_path
        self.profile_dir = profile_dir
        self.game_url = game_url
        self.interval_sec = float(interval_sec)
        self.mode = mode
        self.skip_if_active = skip_if_active
        self.auto_reopen = auto_reopen
        self.headless = headless
        self._log = log
        self._status = status or (lambda s: None)
        self._stop = threading.Event()
        self._thread = None
        self.browser = None
        self.state = "off"

    # -- public API ---------------------------------------------------------

    def start(self):
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True,
                                        name="ptaa-protection")
        self._thread.start()

    def stop(self):
        self._stop.set()

    def join(self, timeout=None):
        if self._thread:
            self._thread.join(timeout)

    @property
    def running(self):
        return bool(self._thread and self._thread.is_alive())

    def show_game_window(self):
        b = self.browser
        if b and b.connected:
            try:
                b.bring_to_front()
                return True
            except Exception:
                pass
        return False

    # -- internals -----------------------------------------------------------

    def _logf(self, msg):
        self._log(msg)

    def _set_state(self, state):
        self.state = state
        self._status(state)

    def _sleep(self, seconds):
        """Interruptible sleep; returns True if stop was requested."""
        return self._stop.wait(seconds)

    def _launch_browser(self):
        exe = find_browser(self.browser_path)
        if not exe:
            raise RuntimeError(
                "No Opera GX, Opera, Chrome or Edge found. Install one of "
                "them, or set 'browser_path' in settings.json.")
        self._log("Starting browser: %s" % os.path.basename(exe))
        b = Browser(exe, self.profile_dir, self.game_url,
                    headless=self.headless, log=self._logf)
        b.launch()
        self.browser = b
        self._log("Browser ready. Remember to log in once - the app "
                  "remembers your session.")

    def _run(self):
        self._set_state("starting")
        try:
            self._launch_browser()
        except Exception as e:
            self._log("ERROR: %s" % e)
            self._set_state("error")
            return
        self._set_state("running")

        first = True
        errors = 0
        while not self._stop.is_set():
            wait = 10.0 if first else self._jittered_interval()
            first = False
            if self._sleep(wait):
                break
            try:
                self._tick()
                errors = 0
            except Exception as e:
                errors += 1
                self._log("Connection hiccup (%s)" % e)
                self._set_state("waiting")
                if errors >= 10:
                    self._log("Too many failures - giving up. Press Start "
                              "to try again.")
                    self._set_state("error")
                    return
                try:
                    self._recover()
                except Exception as e2:
                    self._log("Recovery failed: %s" % e2)
                    self._sleep(10)
        # Stopped: leave the browser open unless asked otherwise.
        self._set_state("off")

    def _jittered_interval(self):
        return self.interval_sec * random.uniform(0.75, 1.25)

    def _recover(self):
        b = self.browser
        if b is None:
            self._launch_browser()
            return
        if b.running or b.is_up():
            # Browser itself is fine; just make sure we re-attach to the tab.
            if not b.ensure_tab(timeout=10):
                self._log("Game tab not found yet - will keep retrying.")
            return
        if not self.auto_reopen:
            self.browser = None
            raise RuntimeError("game window is closed (auto-reopen is off)")
        self._log("Game window was closed - reopening it...")
        try:
            b.shutdown(kill=False)
        except Exception:
            pass
        self._launch_browser()

    def _tick(self):
        b = self.browser
        if b is None:
            return
        if not b.connected:
            if not b.ensure_tab():
                self._log("Game tab not found - open %s in the game window."
                          % self.game_url)
                self._set_state("waiting")
                return
        self._set_state("running")

        probe = b.evaluate(PROBE_JS)
        if not isinstance(probe, dict):
            self._log("Game page is not responding yet.")
            return

        if self.domain_missing(b, probe):
            self._log("The game tab is on another page (%s). Going back to "
                      "%s ..." % (probe.get("url", "?"), self.game_url))
            b.navigate(self.game_url)
            return

        # Skip if the user is genuinely active.
        if self.skip_if_active:
            away_for = probe["now"] - probe["last"]
            own = getattr(self, "_own_last", None)
            looks_own = (own is not None
                         and abs(probe["last"] - own) < OWN_ACTION_TOLERANCE_MS)
            if away_for < USER_INACTIVE_AFTER_MS and not looks_own:
                self._log("You're actively playing - keep-alive skipped.")
                return

        mode = self.mode
        if mode == "step" and (probe["typing"] or not probe["canvas"]):
            reason = ("chat box is focused" if probe["typing"]
                      else "game canvas not found")
            self._log("%s - using a mouse wiggle instead this time." % reason)
            mode = "wiggle"
        elif mode == "both":
            mode = random.choice(("step", "wiggle"))

        if mode == "step":
            k1, k2 = random.choice(STEP_PAIRS)
            b.key_tap_pair(k1, k2, random.uniform(0.05, 0.09),
                           random.uniform(0.15, 0.25))
            self._log("Keep-alive: tiny step (%s then %s)" % (k1, k2))
        else:
            b.mouse_wiggle()
            self._log("Keep-alive: mouse wiggle")

        self._own_last = probe["now"] + 1000

    @staticmethod
    def domain_missing(browser, probe):
        url = probe.get("url") or ""
        return bool(browser.domain) and browser.domain not in url
