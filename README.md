# 🐴 Pony Town Anti-AFK

A desktop app that keeps you connected to **Pony Town** while you're AFK —
the same idea as a Roblox anti-AFK, built for Pony Town's browser-based
client. No more getting dumped back to the loading screen after ~10 minutes
of idling.

**Two editions live in this folder:**

| | Rust edition (`rust\`) | Python edition (`app\`) |
|---|---|---|
| UI | Iced window with the **live game embedded** (top bar → game → log bar) | Tkinter control panel + separate game window |
| Engine | Own private WebView2 inside the app — nothing else to launch | Drives your installed Opera GX / Chrome / Edge |
| Run | `cargo run --release` (or the built exe) | `Start Pony Anti-AFK.bat` |

Both use the same core trick, so pick whichever you prefer. The **Rust
edition is the primary app** going forward.

---

## Rust edition (embedded game view)

```
┌──────────────────────────────────────────────┐
│ [Start] [Mode] [− 4 min +] [Skip]  ● ON      │  ← iced top bar
├──────────────────────────────────────────────┤
│                                              │
│         live pony.town (WebView2)            │  ← real game, playable
│                                              │
├──────────────────────────────────────────────┤
│ [17:13:24] Keep-alive: tiny step (L then R)  │  ← single log bar
└──────────────────────────────────────────────┘
```

Requirements: Rust (MSVC toolchain) + the WebView2 runtime (every up-to-date
Windows 10/11 has it).

```
cd rust
cargo run --release
```

Log in to pony.town once inside the app's game view — the session persists
in `rust\webview-data\`. Press **Start protection** and go AFK; the app's
own engine (no external browser involved) sends trusted input events on a
randomized schedule. Settings persist in `rust\settings.json`.

The engine talks to its embedded WebView2 over the Chrome DevTools Protocol
— the same channel as the Python edition — so the keep-alive events are
still real browser-level input (`isTrusted == true`).

Automated test (mock page that disconnects after 20 s without trusted input):

```
cargo test --test engine_test -- --nocapture
```

## Python edition (control panel + game window)

1. Double-click **`Start Pony Anti-AFK.bat`**
   (requires Python 3 with tkinter — any standard python.org install works;
   you already have one).
2. Press **Start protection**. A dedicated game window opens.
3. Log in to Pony Town once. The app uses its own browser profile stored in
   the `profile\` folder, so **you stay logged in** between sessions.
4. Leave it running and go AFK. The status turns green:
   *"Protection is ON — you can go AFK"*.

To stop, press **Stop protection** (and tick the option to also close the
game window, if you want).

> Tip: right-click `Start Pony Anti-AFK.bat` → *Pin to taskbar* or create a
> desktop shortcut for one-click access.

## How it works (and why it's safer than auto-clickers)

Pony Town disconnects you when it sees no input for a while (supporter
tiers get a longer timeout — Diamond removes it, which is why this app
exists).

Old auto-clicker scripts fake a `canvas.click()` **from inside the page**.
Those clicks have `isTrusted = false` — the browser itself tells every
script "this event was never typed by a human" — which is how Pony Town
detects and flags clicker users (flagged accounts reportedly get
disconnected even while actively playing).

This app takes a different route:

- It runs Pony Town in its own **Opera GX / Chrome / Edge window** (a
  separate profile, your normal browsing is untouched) and talks to the
  browser over the **Chrome DevTools Protocol**. Opera GX is detected and
  preferred automatically.
- Every few minutes (randomized ±25%, default 4) it sends a **trusted**
  input event through the browser's real input pipeline — the same channel
  your physical keyboard and mouse use, `isTrusted = true`,
  indistinguishable from you.
- The default *"Tiny step"* action taps a movement key for ~50–90 ms and
  then the opposite key: your pony turns/steps out and back to the same
  tile. That's it. You can also pick *"Mouse wiggle"* (invisible) or
  *"Both"*.

Built-in safety rails:

- **Pauses while you're actively playing** — it tracks your real input and
  won't interfere for as long as it sees activity in the last 2 minutes.
- **Never types into chat** — if the chat box is focused it switches to a
  mouse wiggle for that cycle.
- **Never steals focus** — the game window can sit in the background or be
  minimized; nothing flashes on your screen.
- If the game window gets closed accidentally, it can reopen it
  (optional, on by default).

## Settings

Saved to `settings.json` next to this file. The GUI covers everything:

| Setting | Meaning |
|---|---|
| Keep-alive action | Tiny step / mouse wiggle / both |
| Interval | Minutes between keep-alives (2–10, default 4; random jitter applied) |
| Pause while I'm playing | Skip keep-alives while your real input is detected |
| Reopen game window | Relaunch the game window if it gets closed |
| Close window on Stop | Also shut the game browser down when you press Stop |

Extra options (edit `settings.json`):

```json
{
  "game_url": "https://pony.town",
  "browser_path": ""
}
```

- `game_url` — works with Pony Town derivatives too (e.g. `https://ashes.town`).
- `browser_path` — force a specific browser executable. Auto-detection
  prefers Opera GX, then Opera, Chrome, Edge (filesystem + registry).

## Files

```
Start Pony Anti-AFK.bat   ← Python edition: run this
app\                      ← Python application (pure Python stdlib)
  engine.py               browser launch + DevTools/WebSocket driver
  keepalive.py            keep-alive scheduling and safety logic
  gui.py                  the window you interact with
  main.pyw                entry point
rust\                     ← Rust edition (primary)
  src\cdp.rs              DevTools Protocol client (port of engine.py)
  src\keepalive.rs        keep-alive engine (port of keepalive.py)
  src\app.rs              iced GUI: top bar / embedded game view / log bar
  tests\engine_test.rs    automated end-to-end test
profile\                  ← Python edition: created at runtime (login lives here)
rust\webview-data\        ← Rust edition: created at runtime (login lives here)
settings.json             ← Python edition settings
rust\settings.json        ← Rust edition settings
tests\                    ← mock idle page + Python engine test
```

## Testing

The engine is covered by an automated test using a mock page that
disconnects its "player" after 20 s without trusted input:

```
python tests\test_engine.py
```

Verified results: with protection on, the mock never disconnects and every
event it sees is trusted; with protection off, it disconnects (the control
case fails as intended).

## Honest note

Automating idle-prevention is against most games' rules, Pony Town's
included — that's a trade-off you're choosing, not something this README
can wave away. This app is built to behave like a real, bored player
(trusted input, long randomized gaps, tiny movement, zero page-level
scripting), which is the lowest-profile way to do it, but no guarantee is
offered or implied. Use it on an account you can afford to lose.
