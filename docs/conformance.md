# Conformance tests against Livestatus on Nagios

corax claims to answer Livestatus queries like the original. The tools in
`tools/conformance` check that claim on a real site: they run the same queries against
the Livestatus module of Checkmk on Nagios and against corax, both reading the same
configuration and the same log files, and compare the rows.

## How it works

`reference.sh` starts a second Nagios next to the running core. It loads the object
configuration of the site and the Livestatus module, with a socket of its own. This
reference

- runs no checks, sends no notifications and accepts no commands,
- keeps its socket, log, status and lock files in a directory of its own, and refuses
  to start if any path of its configuration still points elsewhere,
- reads the log archive and the current log file of the site through symbolic links.

The running core is not touched. It does not matter whether that core is Nagios or
corax.

`corax --query FILE nagios.cfg` answers Livestatus queries without starting the core:
from the configuration, the retention file and the log files. `compare.sh` gives it the
configuration of the reference, so both sides see the same files.

## Running the tests

As the site user of an OMD site that has the Nagios core installed (Checkmk Community):

```bash
cd tools/conformance
./reference.sh start ~/var/corax-reference

# a period that lies completely in the past, and a host with some history in it
./make-queries.sh "$(date -d '8 days ago 00:00' +%s)" "$(date -d 'yesterday 00:00' +%s)" myhost ~/var/corax-queries

CORAX=/path/to/corax ./compare.sh ~/var/corax-reference ~/var/corax-queries
./reference.sh stop ~/var/corax-reference
```

`compare.sh` prints one line per query and exits with 1 if an answer differs. The
answers stay in `out/` of the reference directory; `*.rows` are the normalized forms
that were compared. Any `*.lql` file with `OutputFormat: json` in the query directory
is run, so site specific queries can be added.

The reference holds the parsed log files in memory. On a site with a large archive it
needs up to a few GB while queries over long periods run.

## What is compared

`make-queries.sh` writes these queries:

| Area | Queries |
|---|---|
| State history (`statehist`) | all hosts, all objects of one host, all objects from the middle of a log file, grouped stats, problems and downtimes, columns of the current objects, no lower time bound |
| Log (`log`) | alerts, notifications, one host, grouped stats |
| Configuration | `hosts`, `services`, `hostgroups`, `servicegroups`, `contacts`, `contactgroups`, `commands`, `timeperiods` |

Not compared:

- **The order of the rows.** Nagios keeps its objects in lists whose order is an
  implementation detail; the Checkmk GUI sorts itself.
- **The order of the keys in dictionary columns** (custom variables, tags, labels). The
  original emits them in the order of a hash table.
- **State columns of `hosts` and `services`.** The reference starts without retention
  and runs no checks, so its states are not those of the site.

## Results

Checkmk 2.5.0p10 Community, Nagios 3.5.1, a production site with 80 hosts and 7,200
services, an archive of 753 log files (3.2 GB), period of one week:

| Query | Rows | Result |
|---|---|---|
| `statehist`, all hosts | 555 | equal |
| `statehist`, one host | 750 | equal |
| `statehist`, all objects from the middle of a file | 37,768 | equal |
| `statehist`, grouped stats | 9,137 | equal |
| `statehist`, problems and downtimes | 4,747 | equal |
| `statehist`, columns of the current objects | 743 | equal |
| `statehist`, no lower time bound (two years of logs) | 31,527 | equal |
| `log`, alerts | 4,073 | equal |
| `log`, notifications | 10,073 | equal |
| `log`, one host | 8,622 | equal |
| `log`, grouped stats | 555 | equal |
| `hosts`, `services` of one host | 80, 105 | equal |
| groups, contacts, commands, time periods | 89 | equal |

The first run found one difference, which is fixed: `contact_groups` listed the groups
in the order of the configuration, Nagios lists them in reverse.

Further queries were compared by hand while `statehist` was written: periods with
downtimes and flapping, a log file with invalid UTF-8, `AuthUser`, nested `Or` and
`Negate`. They are not part of the standard set because they depend on what happened
on a site.
