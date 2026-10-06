"""Tkinter GUI for Pony Town Anti-AFK."""

import json
import os
import queue
import time
import tkinter as tk
from tkinter import ttk, scrolledtext, messagebox

from keepalive import Protection, MODE_LABELS

BASE_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP_DIR = os.path.join(BASE_DIR, "app")
SETTINGS_PATH = os.path.join(BASE_DIR, "settings.json")
PROFILE_DIR = os.path.join(BASE_DIR, "profile")

DEFAULTS = {
    "game_url": "https://pony.town",
    "interval_min": 4,
    "mode": "step",
    "skip_if_active": True,
    "auto_reopen": True,
    "close_on_stop": False,
    "browser_path": "",
}

STATE_TEXT = {
    "off": ("Protection is OFF", "#95a5a6"),
    "starting": ("Starting browser...", "#f39c12"),
    "running": ("Protection is ON - you can go AFK", "#2ecc71"),
    "waiting": ("Waiting for the game tab...", "#f39c12"),
    "error": ("Error - check the log below", "#e74c3c"),
}

LABEL_TO_MODE = {v: k for k, v in MODE_LABELS.items()}
LABELS = list(MODE_LABELS.values())


def load_settings():
    s = dict(DEFAULTS)
    try:
        with open(SETTINGS_PATH, "r", encoding="utf-8") as f:
            s.update(json.load(f))
    except Exception:
        pass
    return s


def save_settings(s):
    try:
        with open(SETTINGS_PATH, "w", encoding="utf-8") as f:
            json.dump(s, f, indent=2)
    except Exception:
        pass


class App(tk.Tk):
    def __init__(self):
        super().__init__()
        self.title("Pony Town Anti-AFK")
        self.resizable(False, False)
        self.configure(bg="#1e1f24")
        self.settings = load_settings()
        self.protection = None
        self._ui_queue = queue.Queue()

        outer = ttk.Frame(self, padding=14, style="TFrame")
        outer.grid(sticky="nsew")

        header = ttk.Frame(outer, style="TFrame")
        header.grid(row=0, column=0, sticky="we")
        ttk.Label(header, text="\U0001F40E Pony Town Anti-AFK",
                  style="Header.TLabel").pack(side="left")

        self.status_dot = tk.Canvas(header, width=14, height=14,
                                    highlightthickness=0, bg="#1e1f24")
        self.status_dot.pack(side="right", padx=(0, 6), pady=4)
        self.status_dot_dot = self.status_dot.create_oval(2, 2, 12, 12,
                                                          fill="#95a5a6",
                                                          outline="")
        self.status_label = ttk.Label(header, text=STATE_TEXT["off"][0],
                                      style="Status.TLabel")
        self.status_label.pack(side="right", padx=6)

        ttk.Separator(outer).grid(row=1, column=0, sticky="we", pady=10)

        buttons = ttk.Frame(outer, style="TFrame")
        buttons.grid(row=2, column=0, sticky="we")
        self.toggle_btn = ttk.Button(buttons, text="Start protection",
                                     command=self.toggle, width=22)
        self.toggle_btn.pack(side="left")
        self.show_btn = ttk.Button(buttons, text="Show game window",
                                   command=self.show_game, width=22)
        self.show_btn.pack(side="left", padx=8)

        options = ttk.Frame(outer, style="TFrame")
        options.grid(row=3, column=0, sticky="we", pady=(12, 0))

        row1 = ttk.Frame(options, style="TFrame")
        row1.pack(fill="x")
        ttk.Label(row1, text="Keep-alive action:").pack(side="left")
        self.mode_var = tk.StringVar(
            value=MODE_LABELS.get(self.settings.get("mode", "step"),
                                  LABELS[0]))
        self.mode_box = ttk.Combobox(row1, textvariable=self.mode_var,
                                     values=LABELS, state="readonly", width=26)
        self.mode_box.pack(side="left", padx=8)
        self.mode_box.bind("<<ComboboxSelected>>", lambda e: self.persist())

        row2 = ttk.Frame(options, style="TFrame")
        row2.pack(fill="x", pady=(8, 0))
        ttk.Label(row2, text="Send keep-alive every").pack(side="left")
        self.interval_var = tk.IntVar(value=int(self.settings.get("interval_min", 4)))
        self.interval_box = ttk.Spinbox(row2, from_=2, to=10, width=4,
                                        textvariable=self.interval_var,
                                        command=self.persist)
        self.interval_box.pack(side="left", padx=6)
        ttk.Label(row2, text="minutes (2-10)").pack(side="left")

        self.skip_var = tk.BooleanVar(value=bool(self.settings.get("skip_if_active", True)))
        ttk.Checkbutton(options, text="Pause while I'm actively playing "
                                      "(recommended)", variable=self.skip_var,
                        command=self.persist).pack(anchor="w", pady=(10, 0))
        self.reopen_var = tk.BooleanVar(value=bool(self.settings.get("auto_reopen", True)))
        ttk.Checkbutton(options, text="Reopen the game window if it gets closed",
                        variable=self.reopen_var,
                        command=self.persist).pack(anchor="w")
        self.close_var = tk.BooleanVar(value=bool(self.settings.get("close_on_stop", False)))
        ttk.Checkbutton(options, text="Close the game window when I press Stop",
                        variable=self.close_var,
                        command=self.persist).pack(anchor="w")

        ttk.Label(outer, text="The game window can stay in the background - "
                              "it never needs focus, and nothing appears on "
                              "your screen.",
                  style="Hint.TLabel", wraplength=460,
                  justify="left").grid(row=4, column=0, sticky="w", pady=(10, 0))

        ttk.Separator(outer).grid(row=5, column=0, sticky="we", pady=10)
        ttk.Label(outer, text="Activity log", style="Status.TLabel").grid(
            row=6, column=0, sticky="w")
        self.log_text = scrolledtext.ScrolledText(
            outer, width=66, height=12, state="disabled", bg="#141519",
            fg="#d8d8d8", insertbackground="#d8d8d8", relief="flat",
            font=("Consolas", 9))
        self.log_text.grid(row=7, column=0, sticky="we", pady=(4, 0))

        self._set_state("off")
        self.after(150, self._poll_queue)
        self.protocol("WM_DELETE_WINDOW", self.on_close)

    # -- plumbing ------------------------------------------------------------

    def _post(self, fn):
        self._ui_queue.put(fn)

    def _poll_queue(self):
        try:
            while True:
                fn = self._ui_queue.get_nowait()
                try:
                    fn()
                except Exception:
                    pass
        except queue.Empty:
            pass
        self.after(150, self._poll_queue)

    def log(self, msg):
        self._post(lambda: self._append_log(msg))

    def _append_log(self, msg):
        stamp = time.strftime("%H:%M:%S")
        self.log_text.configure(state="normal")
        self.log_text.insert("end", "[%s]  %s\n" % (stamp, msg))
        if int(self.log_text.index("end-1c").split(".")[0]) > 500:
            self.log_text.delete("1.0", "20.0")
        self.log_text.see("end")
        self.log_text.configure(state="disabled")

    def _set_state(self, state):
        text, color = STATE_TEXT.get(state, STATE_TEXT["off"])
        self.status_label.configure(text=text)
        self.status_dot.itemconfigure(self.status_dot_dot, fill=color)
        self.toggle_btn.configure(
            text="Stop protection" if state in ("running", "starting", "waiting")
            else "Start protection")

    def status(self, state):
        self._post(lambda: self._set_state(state))

    # -- actions ---------------------------------------------------------------

    def current_settings(self):
        mode = LABEL_TO_MODE.get(self.mode_var.get(), "step")
        try:
            interval = max(2, min(10, int(self.interval_var.get())))
        except Exception:
            interval = 4
        return {"game_url": self.settings.get("game_url", DEFAULTS["game_url"]),
                "interval_min": interval, "mode": mode,
                "skip_if_active": bool(self.skip_var.get()),
                "auto_reopen": bool(self.reopen_var.get()),
                "close_on_stop": bool(self.close_var.get()),
                "browser_path": self.settings.get("browser_path", "")}

    def persist(self, *_):
        self.settings = self.current_settings()
        save_settings(self.settings)

    def toggle(self):
        if self.protection and self.protection.running:
            close = bool(self.close_var.get())
            self.log("Stopping protection%s"
                     % (" and closing game window..." if close else "..."))
            self.protection.stop()
            self.protection.join(timeout=10)
            if close and self.protection.browser:
                self.protection.browser.shutdown(kill=True)
            self.protection = None
            self._set_state("off")
            return
        s = self.current_settings()
        self.persist()
        self.protection = Protection(
            browser_path=s["browser_path"] or None,
            profile_dir=PROFILE_DIR,
            game_url=s["game_url"],
            interval_sec=s["interval_min"] * 60,
            mode=s["mode"],
            skip_if_active=s["skip_if_active"],
            auto_reopen=s["auto_reopen"],
            log=self.log, status=self.status)
        self.log("Starting protection (every %d min, mode: %s)"
                 % (s["interval_min"], MODE_LABELS[s["mode"]]))
        self.protection.start()

    def show_game(self):
        if not (self.protection and self.protection.show_game_window()):
            self.log("Game window is not running - press Start first.")

    def on_close(self):
        if self.protection and self.protection.running:
            if not messagebox.askyesno(
                    "Quit", "Protection is still running.\nStop it and quit?"):
                return
            self.protection.stop()
            self.protection.join(timeout=5)
            if bool(self.close_var.get()) and self.protection.browser:
                self.protection.browser.shutdown(kill=True)
        self.destroy()


def run():
    App().mainloop()


if __name__ == "__main__":
    run()
