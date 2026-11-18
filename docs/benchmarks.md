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

## Memory

corax itself uses about 3–4 MB RSS for a small site. The check helper is one Python
process of roughly 80 MB RSS; forked checks share its pages copy-on-write.
