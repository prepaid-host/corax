# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.0] - 2026-11-20

### Added

- Livestatus `rrddata` and `host_rrddata` columns: historic metrics from the PNP4Nagios
  RRDs via rrdcached. The Checkmk GUI needs them for every graph, predictive levels
  need them for their history.

## [0.1.0] - 2026-11-19

### Added

- Nagios 3 compatible configuration loader (templates, additive inheritance, custom
  variables, groups, time periods with date exceptions).
- Engine with soft/hard state machine, flap detection, acknowledgements, comments,
  fixed/flexible/triggered downtimes, notifications, event handlers, freshness checks.
- Async check executor with direct exec, process-group timeouts and bounded concurrency.
- Pre-forking Python check helper for Checkmk host checks with automatic fallback.
- Check result spool reader, external command FIFO and Livestatus `COMMAND`.
- Livestatus server compatible with the Checkmk GUI.
- `nagios.log` with archive rotation, Nagios `retention.dat`, PNP4Nagios npcdmod perfdata.
- `corax-omd` to switch an OMD site between Nagios and corax.
- Exclusive lock on `lock_file`: a second core instance for the same site refuses to start.
- Hardening: bounded request/line sizes, filter nesting and regex size limits in
  Livestatus; user supplied text in notification macros is cleaned like plugin output.
