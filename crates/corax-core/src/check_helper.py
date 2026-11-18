#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""corax check helper ("zygote") for Checkmk precompiled host checks.

Checkmk's "Check_MK" service runs ``python3 -P .../host_checks/<host>`` once per host and
interval. Most of the CPU time of such a run is spent importing the Checkmk check engine
and the check plug-ins (~1.5 CPU seconds per run on a typical site).

This helper imports all of that exactly once and then forks a pre-warmed child for every
host check request. The child executes the unmodified precompiled host check with
``runpy`` (exactly like ``python3 -P <file>`` would) and returns exit code and standard
output to the corax core.

Protocol (one Unix stream connection per check):

    corax  -> helper : {"argv": ["python3", "-P", "<file>", ...]}\n
    helper -> corax  : PID <pid>\n                (child pid == process group id)
    helper -> corax  : {"rc": <int>, "output": "<stdout>"}\n

The core kills the child's process group on timeout and falls back to a plain exec
whenever the helper is not available.
"""

import gc
import importlib
import io
import json
import os
import runpy
import signal
import socket
import sys
import traceback

MAX_OUTPUT = 1024 * 1024


def log(msg: str) -> None:
    sys.stderr.write(f"corax-check-helper[{os.getpid()}]: {msg}\n")
    sys.stderr.flush()


def warm_up(host_checks_dir: str) -> None:
    """Import everything a host check will need, using the precompiled files themselves."""
    try:
        files = sorted(
            os.path.join(host_checks_dir, f)
            for f in os.listdir(host_checks_dir)
            if not f.endswith(".py") and not f.startswith(".")
        )
    except OSError as exc:
        log(f"no host checks to warm up from ({exc})")
        return
    modules: set[str] = set()
    loaded = 0
    for path in files:
        try:
            namespace = runpy.run_path(path, run_name="corax_warmup")
        except BaseException as exc:  # noqa: BLE001 - a broken file must not kill the helper
            log(f"warm-up of {path} failed: {exc!r}")
            continue
        loaded += 1
        config = namespace.get("CONFIG")
        for location in getattr(config, "locations", ()) or ():
            module = getattr(location, "module", None)
            if module and module not in modules:
                modules.add(module)
                try:
                    importlib.import_module(module)
                except Exception as exc:  # noqa: BLE001
                    log(f"cannot pre-import {module}: {exc!r}")
    log(f"warm-up complete: {loaded} host check files, {len(modules)} plug-in modules")


def read_line(conn: socket.socket) -> bytes:
    buf = bytearray()
    while not buf.endswith(b"\n"):
        chunk = conn.recv(65536)
        if not chunk:
            break
        buf += chunk
    return bytes(buf)


def run_child(conn: socket.socket) -> None:
    """Executed in the forked child. Never returns."""
    rc = 3
    try:
        signal.signal(signal.SIGCHLD, signal.SIG_DFL)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        os.setpgid(0, 0)
        request = json.loads(read_line(conn) or b"{}")
        argv = request["argv"]
        # argv = [python3, -P, <file>, *options]
        script = argv[2]
        conn.sendall(f"PID {os.getpid()}\n".encode())

        out_fd = os.memfd_create("corax-check-output")
        os.dup2(out_fd, 1)
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 2)
        sys.stdout = io.TextIOWrapper(io.FileIO(1, "w", closefd=False), encoding="utf-8", errors="replace")
        sys.stderr = io.TextIOWrapper(io.FileIO(2, "w", closefd=False), encoding="utf-8", errors="replace")
        sys.argv = [script, *argv[3:]]
        try:
            runpy.run_path(script, run_name="__main__")
            rc = 0
        except SystemExit as exc:
            code = exc.code
            if code is None:
                rc = 0
            elif isinstance(code, int):
                rc = code
            else:
                sys.stdout.write(f"{code}\n")
                rc = 1
        except BaseException:  # noqa: BLE001
            sys.stdout.write(f"UNKNOWN - corax check helper: exception in host check\n{traceback.format_exc()}")
            rc = 3
        sys.stdout.flush()
        os.lseek(out_fd, 0, os.SEEK_SET)
        output = os.read(out_fd, MAX_OUTPUT).decode("utf-8", errors="replace")
        conn.sendall(json.dumps({"rc": rc, "output": output}).encode() + b"\n")
    except BaseException as exc:  # noqa: BLE001
        try:
            conn.sendall(json.dumps({"rc": 3, "output": f"UNKNOWN - corax check helper failed: {exc!r}"}).encode() + b"\n")
        except OSError:
            pass
    finally:
        os._exit(0)


def main() -> int:
    if len(sys.argv) != 3:
        sys.stderr.write("usage: check_helper.py <socket> <host_checks_dir>\n")
        return 2
    socket_path, host_checks_dir = sys.argv[1], sys.argv[2]

    warm_up(host_checks_dir)
    # Freeze everything allocated so far into the permanent generation: forked children
    # then do not touch (and copy) these pages during garbage collection.
    gc.collect()
    gc.freeze()

    try:
        os.unlink(socket_path)
    except FileNotFoundError:
        pass
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(socket_path)
    os.chmod(socket_path, 0o600)
    server.listen(1024)
    # Children are reaped automatically; the core watches them via their pid.
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)
    log(f"ready on {socket_path}")
    print("READY", flush=True)

    while True:
        conn, _ = server.accept()
        try:
            pid = os.fork()
        except OSError as exc:
            log(f"fork failed: {exc!r}")
            conn.close()
            continue
        if pid == 0:
            server.close()
            run_child(conn)
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
