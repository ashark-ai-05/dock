#!/usr/bin/env python3
"""Run Dock on one owned PTY and prove that it restores that PTY's termios."""

import argparse
import ctypes
import fcntl
import json
import os
import pty
import re
import selectors
import signal
import struct
import subprocess
import sys
import termios
import time


CSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
OSC = re.compile(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
ALT_ENTER = b"\x1b[?1049h"
ALT_LEAVE = b"\x1b[?1049l"


if sys.platform != "darwin":
    raise SystemExit("smoke-slice61-pty.py is macOS-only")


class DarwinTermios(ctypes.Structure):
    _fields_ = [
        ("c_iflag", ctypes.c_ulong),
        ("c_oflag", ctypes.c_ulong),
        ("c_cflag", ctypes.c_ulong),
        ("c_lflag", ctypes.c_ulong),
        ("c_cc", ctypes.c_ubyte * 20),
        ("c_ispeed", ctypes.c_ulong),
        ("c_ospeed", ctypes.c_ulong),
    ]


LIBC = ctypes.CDLL(None, use_errno=True)
LIBC.tcgetattr.argtypes = (ctypes.c_int, ctypes.POINTER(DarwinTermios))
LIBC.tcgetattr.restype = ctypes.c_int


def read_termios(fd):
    state = DarwinTermios()
    if LIBC.tcgetattr(fd, ctypes.byref(state)) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))
    return state


def fields(state):
    return {
        "iflag": state.c_iflag,
        "oflag": state.c_oflag,
        "cflag": state.c_cflag,
        "lflag": state.c_lflag,
        "cc": bytes(state.c_cc),
        "ispeed": state.c_ispeed,
        "ospeed": state.c_ospeed,
    }


def describe(value):
    if isinstance(value, bytes):
        return value.hex()
    return f"0x{value:x}"


def child_process_group():
    os.setpgid(0, 0)


def semantic_evidence(transcript, session, prior_result):
    """Assert UI semantics from the bytes Dock actually rendered on its PTY."""
    data = open(transcript, "rb").read()
    if ALT_ENTER not in data or ALT_LEAVE not in data:
        raise RuntimeError(
            "rendered output did not contain alternate-screen enter and leave bytes"
        )

    # Cursor-positioning sequences may divide a value in the capture even though the
    # terminal displays it continuously. Removing CSI/OSC controls reconstructs that
    # display stream without obtaining state through a separate product interface.
    display = CSI.sub(b"", OSC.sub(b"", data)).decode("utf-8", errors="replace")
    workspaces = re.findall(r"\bworkspace ([0-9]+)\b", display)
    # A complete repaint contains the title; an incremental repaint can update only the
    # title's status cell, immediately followed in the capture by the pane body.
    statuses = re.findall(r"(?:terminal · )?(running)(?:\s+repository:|\s+─)", display)
    runs = re.findall(r"\brun: (dock_ui_[0-9]+)\b", display)
    bindings = re.findall(r"\bbinding: (workspace_[0-9]+/pane_[0-9]+)\b", display)
    if not workspaces:
        raise RuntimeError("rendered output did not visibly show a workspace")
    if not statuses:
        raise RuntimeError("rendered output did not visibly show the selected pane running")
    if not runs or not runs[-1]:
        raise RuntimeError("rendered output did not visibly show a Dock-owned run identifier")
    if not bindings or not bindings[-1]:
        raise RuntimeError("rendered output did not visibly show a runtime binding")

    evidence = {
        "session": session,
        "source": "pty-rendered-output",
        "alternate_screen": {"enter": True, "leave": True},
        "visible": {
            "workspace": workspaces[-1],
            "selected_pane_status": statuses[-1],
            "run": runs[-1],
            "binding": bindings[-1],
        },
    }
    if session == "second":
        if not prior_result:
            raise RuntimeError("second session requires first-session semantic evidence")
        with open(prior_result, encoding="utf-8") as source:
            prior = json.load(source)
        for name in ("workspace", "run", "binding"):
            if evidence["visible"][name] != prior["visible"][name]:
                raise RuntimeError(
                    f"reconnect did not visibly preserve {name}: "
                    f"first={prior['visible'][name]!r} second={evidence['visible'][name]!r}"
                )
    return evidence


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dock", required=True)
    parser.add_argument("--keys", required=True)
    parser.add_argument("--transcript", required=True)
    parser.add_argument("--error-log", required=True)
    parser.add_argument("--result", required=True)
    parser.add_argument("--session", choices=("first", "second"), required=True)
    parser.add_argument("--prior-result")
    args = parser.parse_args()

    master_fd, slave_fd = pty.openpty()
    proc = None
    stage = "initializing owned PTY"
    try:
        # Establish the viewport before the entry snapshot. The parent retains this exact
        # slave descriptor until after wait(), so both snapshots address one terminal object.
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        # The harness plays the shell: it owns the controlling terminal for the lifetime of
        # both snapshots. Dock gets a separate foreground process group, not a throwaway session
        # whose leader exit would revoke the slave on Darwin.
        os.setsid()
        fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        signal.signal(signal.SIGTTOU, signal.SIG_IGN)
        before = fields(read_termios(slave_fd))
        tty_name = os.ttyname(slave_fd)
        environment = os.environ.copy()
        environment["DOCK_TEST_KEY_EVENTS"] = args.keys
        stage = "spawning dock"
        proc = subprocess.Popen(
            [args.dock],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            env=environment,
            close_fds=True,
            preexec_fn=child_process_group,
        )
        os.tcsetpgrp(slave_fd, proc.pid)

        selector = selectors.DefaultSelector()
        stage = "reading dock PTY output"
        os.set_blocking(master_fd, False)
        selector.register(master_fd, selectors.EVENT_READ)
        deadline = time.monotonic() + 30
        with open(args.transcript, "wb") as transcript:
            while proc.poll() is None:
                if time.monotonic() >= deadline:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
                    raise RuntimeError("dock timed out after 30 seconds")
                for key, _ in selector.select(0.1):
                    try:
                        chunk = os.read(key.fd, 65536)
                    except BlockingIOError:
                        continue
                    if chunk:
                        transcript.write(chunk)
            while True:
                try:
                    chunk = os.read(master_fd, 65536)
                except BlockingIOError:
                    break
                if not chunk:
                    break
                transcript.write(chunk)

        returncode = proc.wait()
        stage = "reading post-exit termios"
        os.tcsetpgrp(slave_fd, os.getpgrp())
        after = fields(read_termios(slave_fd))
        differences = [
            f"{name}: before={describe(before[name])} after={describe(after[name])}"
            for name in before
            if before[name] != after[name]
        ]
        if differences:
            raise RuntimeError(
                f"dock did not fully restore owned PTY {tty_name}: " + "; ".join(differences)
            )
        stage = "validating dock exit"
        if returncode != 0:
            raise RuntimeError(f"dock exited with status {returncode}")
        with open(args.transcript, "ab") as transcript:
            transcript.write(b"\nTERMINAL_RESTORED\n")
        stage = "validating rendered UI evidence"
        evidence = semantic_evidence(args.transcript, args.session, args.prior_result)
        with open(args.result, "w", encoding="utf-8") as result:
            json.dump(evidence, result, indent=2, sort_keys=True)
            result.write("\n")
        open(args.error_log, "wb").close()
    except Exception as error:
        error = RuntimeError(f"{stage}: {error}")
        with open(args.error_log, "w", encoding="utf-8") as failure:
            failure.write(f"{error}\n")
        print(error, file=sys.stderr)
        return 1
    finally:
        if proc is not None and proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
        os.close(master_fd)
        os.close(slave_fd)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
