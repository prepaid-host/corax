# The Python check helper

## Problem

Checkmk Community checks every host with an active service called **Check_MK**:

```
$OMD_ROOT/bin/python3 -P $OMD_ROOT/var/check_mk/core/helper_config/latest/host_checks/<host>
```

The file is a precompiled Python module that imports the Checkmk check engine, loads
the plug-ins needed for that host, fetches the agent data, runs all checks and writes the
results of all passive services into the check result spool.

Profiling one such run on a stock site (cProfile, Checkmk 2.5):

| Phase | Time |
|---|---|
| Module imports (`cmk.base.config`, pydantic models, …) | ≈ 0.6–0.9 s |
| Plug-in discovery and import (`load_selected_plugins`) | ≈ 0.8 s |
| Actual checking (excluding waiting for the agent) | small fraction |

Nagios forks a fresh interpreter for every host and interval, so this work is repeated
N times per minute.

## Solution

corax starts a long-running helper process with the site's own Python interpreter
(the same binary named in the command line). The helper

1. executes every precompiled host check file *without* running its `main()`, which
   imports all modules the host checks need, and pre-imports all plug-in modules they
   reference,
2. freezes the garbage collector generations (`gc.freeze()`) so that forked children
   share these pages copy-on-write,
3. listens on a Unix socket and `fork()`s one child per check request. The child runs the
   unmodified host check file with `runpy` exactly like `python3 -P <file>`, captures its
   standard output in a memfd and returns exit code and output.

The core matches only the exact Checkmk host check invocation (`python3 -P
…/host_checks/<host>` with the optional `-v`, `-vv`, `-d` flags). Everything else is
executed normally.

## Safety properties

* **Fallback:** while the helper is starting (warm-up takes a few seconds), after a crash
  or on any protocol error before the check started, the check is executed with plain
  fork/exec. A crashed helper is restarted after a 30 s back-off.
* **Isolation:** each check runs in its own forked process and process group; the core
  kills the group on timeout, exactly like for normal checks.
* **Freshness:** configuration and autochecks are read by the child at run time, as in
  the unmodified file. The helper is restarted on every core reload (`cmk -O`, "Activate
  changes"), so updated plug-ins (e.g. after installing an MKP) are picked up.
* **No changes to Checkmk:** the host check files and the Checkmk code are used as they are.

## Disabling

```
corax --no-check-helper …        # or CORAX_CHECK_HELPER=0
```
