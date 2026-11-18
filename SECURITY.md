# Security policy

## Supported versions

Only the latest release receives security fixes while corax is in the 0.x series.

## Reporting a vulnerability

Please **do not** open a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/prepaid-host/corax/security/advisories/new)
instead. You will receive an acknowledgement within 7 days.

Relevant attack surface:

* the Livestatus socket (query parser, `COMMAND`),
* the external command FIFO,
* the check result spool directory,
* the check helper socket (local, `0600`, site user only).

All of them are local interfaces protected by file permissions of the OMD site; corax
itself never opens a TCP port.
