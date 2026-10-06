"""Entry point for Pony Town Anti-AFK. Run with pythonw for no console window."""

import os
import socket
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

_GUARD_PORT = 57555  # fixed local port used as a single-instance lock


def _acquire_single_instance_lock():
    """Bind a localhost port for the lifetime of the process. If another
    app instance already holds it, report that it is already running."""
    guard = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        guard.bind(("127.0.0.1", _GUARD_PORT))
        guard.listen(1)
        return guard
    except OSError:
        guard.close()
        return None


def main():
    guard = _acquire_single_instance_lock()
    if guard is None:
        import tkinter as tk
        from tkinter import messagebox
        root = tk.Tk()
        root.withdraw()
        messagebox.showinfo(
            "Pony Town Anti-AFK",
            "Pony Town Anti-AFK is already running.\n\n"
            "Look for its window in the taskbar - launching it twice "
            "would run two protections at once.")
        return
    try:
        from gui import run
        run()
    finally:
        guard.close()


if __name__ == "__main__":
    main()
