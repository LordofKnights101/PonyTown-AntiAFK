# Project instructions

## Committing changes (standing rule)

For any new change made in this repository — code, docs, tests, config:

1. Stage everything: `git add -A`
2. Commit with a clear, descriptive message
3. Push immediately: `git push origin main`

Do this in the same working session as the change, so the GitHub mirror
(https://github.com/LordofKnights101/PonyTown-AntiAFK) is always current.

## Never commit runtime data

These are gitignored and must stay that way (they contain login sessions and
build artifacts):

- `profile/` — Python edition's game browser profile
- `rust/webview-data/` — Rust edition's WebView2 profile
- `settings.json`, `rust/settings.json`
- `rust/target/`, `__pycache__/`

## Repo layout

- `app/` + `Start Pony Anti-AFK.bat` — Python edition (control panel + game in
  your own Opera GX/Chrome/Edge)
- `rust/` — Rust edition (primary; iced GUI with the game embedded via wry)
- `tests/` — mock idle page + Python engine test; `rust/tests/engine_test.rs`
  is the Rust engine test
- `health_check.py` — read-only session monitor used by the 10-minute checkup
