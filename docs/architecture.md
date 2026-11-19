# Architecture

## Crates

| Crate | Responsibility |
|---|---|
| `corax-config` | Parses `nagios.cfg`, resource files and object definitions; resolves templates, group membership and host lists into a validated `ObjectConfig`. Pure, synchronous, no global state. |
| `corax-core` | Runtime model (`World`), scheduler, check execution, check result processing (state machine), notifications, downtimes, comments, external commands, retention, `nagios.log`, perfdata, the check result spool and the command FIFO. |
| `corax-livestatus` | Livestatus protocol server on top of `corax-core`. |
| `corax` | The binary: Nagios-compatible CLI, daemonization, signal handling, wiring. |

## Concurrency model

```
                 ┌───────────────────────── Shared ─────────────────────────┐
                 │  RwLock<World>   paths   input channel   change counter  │
                 └──────▲──────────────▲────────────▲───────────────▲───────┘
                        │ write        │ read       │ send          │ watch
                ┌───────┴──────┐  ┌────┴──────┐ ┌───┴────────┐ ┌────┴───────┐
                │    Engine    │  │Livestatus │ │ cmd FIFO / │ │ Livestatus │
                │ (one task)   │  │ readers   │ │ spool      │ │ WaitTrigger│
                └──┬────────▲──┘  └───────────┘ │ reaper     │ └────────────┘
       spawn check │        │ CheckResult       └────────────┘
                ┌──▼────────┴──┐
                │ check tasks  │──► exec / Python check helper
                └──────────────┘
```

* **One writer.** All mutations of the monitoring state happen in the engine task. Check
  executions, the command FIFO, the spool reaper and Livestatus `COMMAND`s only send
  messages (`Input`) to it. This removes data races by construction and makes the state
  machine easy to test synchronously.
* **Batching.** The engine drains up to 10 000 pending inputs per write lock, so a host
  check that delivers 80 passive results costs one lock acquisition.
* **Effects after unlock.** Everything that does I/O — log lines, notifications, event
  handlers, retention — is collected as an `Effect` while the lock is held and executed
  after it is released. Livestatus readers are therefore never blocked by disk or process
  creation.
* **Scheduler.** A binary heap keyed by due time. Rescheduling bumps a per-object
  generation number; stale heap entries are skipped when popped (O(log n), no
  decrease-key). Initial checks are spread deterministically over the check interval.
* **Execution.** Each check runs in its own process group with a timeout that kills the
  whole group. Command lines without shell meta characters are executed directly
  (no `/bin/sh`); simple quoting is handled by corax. Concurrency is bounded by a
  semaphore (`max_concurrent_checks`).
* **Reload** (`SIGHUP`, `cmk -O`): the configuration is loaded and validated off the
  engine loop, a new `World` is built, the runtime state of the old world is carried over
  through the retention serializer, and the world is swapped atomically. Checks that are
  still running report against the new world by name (object ids are only valid within a
  configuration *epoch*).

## Checkmk integration points

| Integration | How corax handles it |
|---|---|
| `~/etc/init.d/nagios` | Only `BIN=` is changed (by `corax-omd install`). Start/stop/reload/verify keep their OMD semantics. |
| `cmk -R` / `cmk -O` | Unchanged: Checkmk validates with the real `nagios -vp`, then calls the init script. |
| Object configuration | Read directly, including Checkmk's hex-encoded label/tag custom variables. |
| "Check_MK" service (`python3 -P …/host_checks/<host>`) | Executed through the [check helper](check-helper.md) when possible. |
| Passive results | Check result spool (`check_submission = "file"`, default) and `PROCESS_*_CHECK_RESULT` commands. |
| Livestatus | Socket path and options taken from the `livestatus.o` `broker_module` line. |
| Graphs | `npcdmod.o` options (`perfdata_file`, `perfdata_spool_dir`, interval) are honoured natively. Graph data is served by the `rrddata` Livestatus columns, which read the RRDs through the site's rrdcached after the world lock has been released. |
| Notifications | Contacts and notification commands from the configuration, e.g. `check-mk-notify` → `cmk --notify`. |
| History | `nagios.log` and `archive/` in Nagios format, consumed by Livestatus `log`/`statehist`. |
