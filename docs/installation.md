# Installation and operation

## Requirements

* Checkmk Community (2.2 or newer) or another OMD-based site using the Nagios core
  (`omd config show CORE` prints `nagios`).
* Linux x86_64 or aarch64. The release binaries are statically linked (musl) and run on
  every distribution supported by Checkmk.

## Install

```bash
omd su mysite
tar xzf corax-x86_64-linux.tar.gz
./corax-omd install --binary ./corax
```

`corax-omd install`

1. checks that the site uses the Nagios core,
2. loads the current site configuration with corax and aborts without changes if it
   cannot be loaded,
3. copies the binary to `~/local/bin/corax`,
4. backs up `~/etc/init.d/nagios` to `~/etc/init.d/nagios.corax-backup` and changes
   only its `BIN=` line,
5. restarts the core.

From then on `omd start|stop|restart|reload nagios`, `cmk -R`, `cmk -O` and "Activate
changes" in the GUI operate corax.

## Uninstall / switch back

```bash
corax-omd uninstall
```

Restores the original init script and starts Nagios. Because corax reads and writes
Nagios' `retention.dat`, states, acknowledgements, comments and downtimes are preserved
in both directions.

## Site updates (`omd update`)

`omd update` notices that `etc/init.d/nagios` was modified. Either

* run `corax-omd uninstall` before and `corax-omd install` after the update (recommended), or
* accept the new version of the file during the update and run `corax-omd install` again.

## Monitoring the core itself

`omd update` may restore the init script of the Nagios core, and the site then runs
Nagios again without telling anyone. `corax-site-check` is a Checkmk local check that
makes this visible. Copy it from the release package to the agent of the host that runs
the site:

```bash
install -m 0755 corax-site-check /usr/lib/check_mk_agent/local/
```

After the next service discovery every site has a service `corax <site>`:

| State | Meaning |
|---|---|
| CRIT | the site does not run corax, or the core does not answer |
| WARN | a newer release is available, or the installed binary is newer than the running core (restart pending) |
| OK | corax is running in the installed version |

Optional settings in `/etc/check_mk/corax.cfg`:

```
SITES=site1 site2      # default: all sites of `omd sites`
CONTAINER=name         # the sites live in this Docker container
CHECK_UPDATES=no       # do not ask GitHub for the newest release
```

The newest release is looked up through the GitHub API at most every six hours.

## Options

corax accepts the Nagios command line. Additional options can be set by editing the
`OPTIONS=` line of the init script or via environment variables in `~/etc/environment`:

| Option | Environment | Meaning |
|---|---|---|
| `--no-check-helper` | `CORAX_CHECK_HELPER=0` | Execute Checkmk host checks with plain fork/exec |
| `--max-concurrent-checks N` | – | Upper limit of concurrently running checks (default: `max_concurrent_checks` from `nagios.cfg`, `0` = 2048) |
| `--log-level LEVEL` | `RUST_LOG` | Verbosity of the diagnostic log |

## Files

| File | Purpose |
|---|---|
| `~/var/nagios/nagios.log`, `~/var/nagios/archive/` | Monitoring history (Nagios format) |
| `~/var/nagios/corax.log` | Diagnostic log of corax itself |
| `~/var/nagios/retention.dat` | State retention (Nagios format) |
| `~/tmp/nagios/corax-check-helper.{py,sock}` | Check helper script and socket (recreated at runtime) |
| `~/tmp/run/live` | Livestatus socket |
| `~/tmp/run/nagios.cmd` | External command FIFO |

## Troubleshooting

* `omd status nagios` / `corax-omd status` — is the core running, which core is active?
* `tail -f ~/var/nagios/corax.log` — diagnostic messages (configuration warnings, check
  helper state, reload results).
* `~/local/bin/corax -v ~/tmp/nagios/nagios.cfg` — verify the configuration the way the
  init script does.
* Run in the foreground for debugging: `omd stop nagios; ~/local/bin/corax --log-level debug ~/tmp/nagios/nagios.cfg`.
