# corax

**A high-performance, Nagios-compatible monitoring core for Checkmk Community sites, written in Rust.**

corax is a drop-in replacement for the Nagios 3 core that Checkmk Community (formerly "Raw
Edition") ships with. It reads the very same configuration, speaks Livestatus on the very
same socket, consumes the same check result spool and command pipe, and writes the same
`nagios.log`, `retention.dat` and PNP4Nagios perfdata. The Checkmk GUI, REST API,
`cmk -R`/`cmk -O` and the notification system keep working unchanged — they simply talk to
a faster core.

> **Status: alpha.** corax runs a production Checkmk site with 80 hosts and 7,200
> services, but it has not seen broad production use yet. Switching back to Nagios is a
> single command and keeps all state.

## Why

On Checkmk Community, every host is checked by a dedicated `python3` process that Nagios
forks once per check interval. Profiling a stock 2.5 site shows that **more than 90 % of the
CPU time of such a run is spent importing the Checkmk check engine and its plug-ins**
(≈1.5 CPU seconds per host and minute), before a single check is executed. Add Nagios 3's
single-threaded event loop and its one-second check result reaper, and larger sites spend
most of their hardware on overhead.

corax addresses this on three levels:

| Area | Nagios 3 | corax |
|---|---|---|
| Checkmk host checks | fork + exec a fresh Python interpreter per host and interval | **pre-warmed Python helper** forks an already-initialised interpreter (no import cost), automatic fallback to exec |
| Check execution | single-threaded event loop, `/bin/sh` for most commands | async executor on all cores, direct `execve` whenever no shell is needed, per-check process groups |
| Result processing | spool reaped once per second | spool reaped immediately after a host check finishes (≤ 250 ms otherwise), batched state updates |
| Livestatus | NEB module sharing Nagios' global lock | native multi-threaded server reading a lock-light in-memory model |

## Features

- **Drop-in**: same CLI as Nagios (`-v`, `-pv`, `-ud`), reads `nagios.cfg`, `cfg_dir`,
  resource files and all object types Checkmk generates (templates, additive inheritance,
  custom variables, host/service groups, contacts, time periods with exceptions, host
  and service dependencies).
- **Livestatus** on the configured socket: all tables used by the Checkmk GUI, filters,
  stats, group-by, `AuthUser`, `WaitCondition`, `COMMAND`, `fixed16` headers, JSON/Python
  output, Checkmk labels and tags.
- **Everything the GUI shows**: graphs and predictive levels (`rrddata` columns read from
  the RRDs), availability reports (`statehist`), the Event Console views (forwarded to
  mkeventd).
- **Nagios semantics**: soft/hard states, retries, flap detection, acknowledgements
  (sticky/persistent), fixed/flexible/triggered downtimes, comments, notifications with
  all standard macros (including `check-mk-notify`), event handlers, freshness checks,
  parent/child reachability.
- **Lossless switching**: retention is read and written in Nagios' `retention.dat` format.
- **History and graphs**: Nagios-compatible `nagios.log` with archive rotation;
  PNP4Nagios `npcdmod`-compatible perfdata spooling.
- **Single static binary**, no runtime dependencies.

See [docs/compatibility.md](docs/compatibility.md) for the full matrix and the known gaps.

## Performance

Same Checkmk Community 2.5 site, 100 hosts / 1 600 services, 1-minute interval
([methodology](docs/benchmarks.md)):

| Core | CPU | Host checks done (of 200 due) | CPU per host check |
|---|---|---|---|
| Nagios 3.5.1 | 164 % of a core | 108 | 1 824 ms |
| **corax** | **35 % of a core** | **210** | **207 ms** |

On a production site (80 hosts, 7,200 services, Checkmk 2.5.0p10), measured for ten
minutes each:

| | Nagios 3.5.1 | corax |
|---|---|---|
| CPU of the whole site container | 1.35 cores | **0.38 cores** |
| Delay until a due check starts (average / maximum) | 141 ms / 1.44 s | **1.7 ms / 11 ms** |
| Availability of all objects over 30 days (Livestatus `statehist`) | 2.2 s | **1.0 s** |

## Verified against the original

Compatibility is tested, not assumed: [conformance tests](docs/conformance.md) run the
same Livestatus queries against Checkmk's Livestatus on Nagios and against corax, on the
same configuration and log files, and compare the rows. On the site above, all queries
of the standard set return the same rows, including 37,768 rows of state history.

## Quick start (Checkmk Community / OMD site)

```bash
# as the site user
omd su mysite
curl -LO https://github.com/prepaid-host/corax/releases/latest/download/corax-x86_64-linux.tar.gz
tar xzf corax-x86_64-linux.tar.gz
./corax-omd install --binary ./corax      # verifies the config, switches the core, restarts it
```

Switch back at any time:

```bash
corax-omd uninstall
```

Details: [docs/installation.md](docs/installation.md).

## Building from source

```bash
cargo build --release                                        # dynamically linked
cargo build --release --target x86_64-unknown-linux-musl     # fully static (recommended)
cargo test --workspace
```

Rust 1.85 or newer is required. See [docs/development.md](docs/development.md).

## Documentation

- [Architecture](docs/architecture.md)
- [Installation and operation](docs/installation.md)
- [Compatibility matrix](docs/compatibility.md)
- [Conformance tests](docs/conformance.md)
- [The Python check helper](docs/check-helper.md)
- [Benchmarks](docs/benchmarks.md)
- [Development guide](docs/development.md)

## Contributing

Contributions are welcome — please read [CONTRIBUTING.md](CONTRIBUTING.md) first. Security
issues: see [SECURITY.md](SECURITY.md).

## License

corax is licensed under the [GNU General Public License v2.0 or later](LICENSE), like the
Nagios core and Checkmk it interoperates with.

corax is an independent project. It is not affiliated with or endorsed by Checkmk GmbH or
Nagios Enterprises. "Checkmk" and "Nagios" are trademarks of their respective owners and
are used here only to describe compatibility.
