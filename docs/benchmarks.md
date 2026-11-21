# Benchmarks

## CPU usage on a Checkmk Community site

Setup: `checkmk/check-mk-community` 2.5.0p20 container, 100 hosts with 16 services each
(1 600 services), 1-minute check interval, 8-core VM. To measure the core and the
Checkmk host checks rather than agent latency, the agent data comes from a static agent
dump (`datasource_programs` → `cat agent.out`). CPU time is the container cgroup's
`usage_usec` over a 120 s window after a 75 s settle period; executed host checks are
counted by sampling `last_check` of every `Check_MK` service via Livestatus every 5 s.

| Core | CPU (share of one core) | Host checks executed in 120 s (expected: 200) | CPU per host check |
|---|---|---|---|
| Nagios 3.5.1 | 164 % | 108 | 1 824 ms |
| corax, check helper disabled | 212 % | 204 | 1 287 ms |
| **corax, check helper enabled** | **35 %** | **210** | **207 ms** |

Observations:

* Nagios does not keep up with the schedule at this size (108 of 200 checks), so its
  absolute CPU number understates its real cost.
* Without the helper, corax executes every check on time; per check it is ~30 % cheaper
  than Nagios (direct `execve` instead of `/bin/sh`, no reaper delays).
* With the check helper, the cost per host check drops by a factor of ~9, because the
  Checkmk check engine and plug-ins are no longer imported for every run.

The script used to produce these numbers is part of the maintainers' tooling; the method
above is sufficient to reproduce them on any site. Numbers on real sites depend heavily
on agent types, plug-ins and SNMP usage — please share your measurements in an issue.

## A production site

Checkmk 2.5.0p10 Community in a container, 80 hosts and 7,200 services, real agents,
1-minute interval. Each core ran for ten minutes; CPU time is the container cgroup's
`usage_usec`, the delay is the `latency` column of the `Check_MK` services.

| | Nagios 3.5.1 | corax |
|---|---|---|
| CPU of the container | 1.35 cores | 0.38 cores |
| Host checks executed | 800 | 789 |
| Delay until a due check starts, average | 141 ms | 1.7 ms |
| Delay until a due check starts, maximum | 1.44 s | 11 ms |
| Run time of a host check, average | 4.64 s | 4.08 s |

The corax measurement began right after the switch, while the checks were still being
spread over the interval, hence the eleven checks fewer. Afterwards all 80 hosts were
checked every minute.

## State history queries

Availability reports read the Livestatus `statehist` table, which is rebuilt from the
log files for every query. Same site, archive of 753 log files (3.2 GB):

| Query | Livestatus on Nagios | corax |
|---|---|---|
| all objects, 7 days (55,246 rows) | 1.3 s | 0.7 s |
| all objects, 30 days, grouped stats | 2.2 s | 1.0 s |
| one host, 1 year | 11.4 s | 3.0 s |

The corax times include 0.25 s for loading the configuration, because the queries ran
through `corax --query`. The Nagios process held 1.8 GB of memory after these queries;
corax needed 212 MB at its peak and keeps nothing afterwards.

## Memory

corax itself uses about 3–4 MB RSS for a small site. The check helper is one Python
process of roughly 80 MB RSS; forked checks share its pages copy-on-write.
