# Compatibility matrix

Reference: Nagios Core 3.5.1 and the Livestatus shipped with Checkmk 2.5 (Community).
Verified end-to-end against `checkmk/check-mk-community` 2.5.0p20.

Legend: ✅ supported · 🟡 partial · ⛔ not supported (accepted and ignored unless stated otherwise)

## Command line and process

| Feature | Status | Notes |
|---|---|---|
| `-v` / `-pv` verify | ✅ | Nagios-like summary, exit code 0/1 |
| `-d` / `-ud` daemon | ✅ | double fork, process group leader (OMD stops the group) |
| `-p`, `-u`, `-s` | ✅ | accepted, no effect |
| `SIGHUP` reload | ✅ | state carried over, failed reload keeps the running config |
| `SIGTERM` / `SIGINT` | ✅ | retention is saved |
| `lock_file` | ✅ | contains the PID |

## Main configuration (`nagios.cfg`)

| Directive | Status |
|---|---|
| `cfg_file`, `cfg_dir`, `resource_file` (`$USERn$`) | ✅ |
| `log_file`, `log_archive_path`, `log_rotation_method` (h/d/w/m) | ✅ |
| `command_file`, `check_result_path`, `max_check_result_file_age` | ✅ |
| `state_retention_file`, `retain_state_information`, `retention_update_interval`, `use_retained_program_state`, `use_retained_scheduling_info` | ✅ |
| `interval_length`, `*_check_timeout`, `notification_timeout`, `event_handler_timeout` | ✅ |
| `max_concurrent_checks`, `max_*_check_spread` | ✅ |
| `enable_notifications`, `execute_*_checks`, `accept_passive_*_checks`, `enable_event_handlers`, `enable_flap_detection`, `process_performance_data` | ✅ |
| flap thresholds, freshness settings, `passive_host_checks_are_soft`, `use_aggressive_host_checking` | ✅ |
| `log_*` switches (initial states, retries, passive checks, external commands, notifications, event handlers) | ✅ |
| `illegal_macro_output_chars`, `global_*_event_handler` | ✅ |
| `broker_module` | 🟡 | Livestatus and npcdmod are implemented natively from their arguments; other NEB modules are not loaded |
| `obsess_over_*`, `ocsp_command`, `ochp_command` | ⛔ |
| `status_file` (`status.dat`) | ⛔ | not used by Checkmk ≥ 2.0 |
| `enable_environment_macros` | ⛔ | Checkmk passes notification data on the command line |

## Objects

| Object / feature | Status |
|---|---|
| `host`, `service`, `command`, `timeperiod`, `contact`, `contactgroup`, `hostgroup`, `servicegroup` | ✅ |
| Templates (`name`, `use` with multiple templates, `register 0`), additive inheritance (`+`), `null` | ✅ |
| Custom variables (`_FOO`), incl. Checkmk hex-encoded labels/tags | ✅ |
| Service host lists (`host_name a,b`, `*`, `!excluded`, `hostgroup_name`) | ✅ |
| Implicit inheritance host → service (contacts, notification period/interval) | ✅ |
| Time periods: weekdays, `YYYY-MM-DD[ - YYYY-MM-DD]` exceptions, `exclude` | ✅ |
| Time periods: `day 1`, `monday 3`, `december 25` style exceptions | ⛔ | warning on load |
| `servicedependency`, `hostdependency` | ✅ | notification and execution criteria, `inherits_parent`, `dependency_period`, host and service lists, hostgroups; `soft_state_dependencies` is honoured. Servicegroup lists are not expanded |
| `*escalation`, `*extinfo` | ⛔ | warning on load |

## Runtime semantics

| Feature | Status |
|---|---|
| Soft/hard states, retries, `max_check_attempts`, volatile services | ✅ |
| Host reachability (DOWN vs. UNREACHABLE via parents), on-demand host checks | ✅ |
| Flap detection (21 state history, weighted) incl. comments and notifications | ✅ |
| Acknowledgements (normal/sticky, persistent, notify) | ✅ |
| Downtimes: fixed, flexible, triggered, propagated to child hosts, host+services | ✅ |
| Comments (user, downtime, flapping, acknowledgement) | ✅ |
| Notifications: problem, recovery, acknowledgement, flapping, downtime, custom; re-notification interval; first notification delay; contact options and periods | ✅ |
| Notification escalations | ⛔ |
| Event handlers (object and global) | ✅ |
| Freshness checks | ✅ |
| Check periods (checks are rescheduled into the next active slot) | ✅ |

## External commands

All commands used by the Checkmk GUI and REST API are implemented, including
`PROCESS_{HOST,SERVICE}_CHECK_RESULT`, `SCHEDULE[_FORCED]_{HOST,SVC}_CHECK`,
`SCHEDULE[_FORCED]_HOST_SVC_CHECKS`, `ACKNOWLEDGE_*[_EXPIRE]`, `REMOVE_*_ACKNOWLEDGEMENT`,
`ADD/DEL_*_COMMENT`, `DEL_ALL_*_COMMENTS`, `SCHEDULE_{HOST,SVC,HOST_SVC}_DOWNTIME`,
`SCHEDULE_AND_PROPAGATE[_TRIGGERED]_HOST_DOWNTIME`, `SCHEDULE_{HOSTGROUP,SERVICEGROUP}_*_DOWNTIME`,
`DEL_*_DOWNTIME`, `DEL_DOWNTIME_BY_HOST_NAME`, `SEND_CUSTOM_*_NOTIFICATION`,
`DELAY_*_NOTIFICATION`, all `ENABLE_*/DISABLE_*` toggles for objects, contacts and the
program, `START/STOP_*`, `SAVE_STATE_INFORMATION`, `RESTART_PROCESS`, `SHUTDOWN_PROCESS`,
`LOG`. Livestatus additionally handles `MK_LOGWATCH_ACKNOWLEDGE`, `DEL_CRASH_REPORT` and
forwards `EC_*` to the Event Console.

`CHANGE_*` commands that modify configuration (check command, intervals, custom
variables) are accepted and ignored: the configuration is owned by Checkmk.

## Livestatus

| Feature | Status |
|---|---|
| Tables `hosts`, `services`, `hostgroups`, `servicegroups`, `contacts`, `contactgroups`, `commands`, `timeperiods`, `status`, `columns`, `comments`, `downtimes`, `log`, `statehist`, `eventconsoleevents`, `eventconsolehistory`, `eventconsolerules`, `eventconsolestatus`, `hostsbygroup`, `servicesbygroup`, `servicesbyhostgroup`, `crashreports`, `labels` | ✅ |
| `statehist` | ✅ | rebuilt from `nagios.log` and the archive with the algorithm of the original, so availability reports match; the same log files give the same rows as Livestatus on Nagios (the order of the rows differs) |
| `eventconsole*` tables | ✅ | forwarded to mkeventd like the original: only the columns of the query are requested, time range and simple filters are evaluated by mkeventd, hosts are joined by name, alias or address, `AuthUser` follows the contact group precedence of the event. `eventconsolereplication` is not provided |
| Headers `Columns`, `Filter`, `And`/`Or`/`Negate`, `Stats*`, group-by, `Limit`, `OrderBy`, `OutputFormat` (json, python, python3, csv, default), `Separators`, `ColumnHeaders`, `ResponseHeader: fixed16`, `KeepAlive`, `AuthUser`, `Wait*` | ✅ |
| `Localtime`, `Timelimit` | 🟡 | accepted, ignored |
| Column set | ✅ | the complete 2.5 schema is known; columns without data in corax return type-correct defaults |
| `rrddata:` dynamic columns (`rrddata`, `host_rrddata`) | ✅ | read from the PNP4Nagios RRDs through rrdcached (`FETCHBIN`), like the original; used by all GUI graphs and by predictive levels |
| Output encoding | ✅ | byte-compatible string escaping and float formatting |
| `livestatus_version` | ✅ | reports the site's Checkmk version (the GUI parses it) |
| `program_version` | 🟡 | `corax-<version>` |

## Files

| File | Status |
|---|---|
| `nagios.log` + archive | ✅ Nagios format, including the lines the Livestatus module adds for the state history (`TIMEPERIOD TRANSITION`, `logging initial states`, downtime alerts after a start or rotation) |
| `retention.dat` | ✅ read and written in Nagios format (switching in both directions) |
| Check result spool (`c??????` + `.ok`) | ✅ |
| PNP4Nagios npcdmod perfdata (`perfdata.dump` → spool) | ✅ |
