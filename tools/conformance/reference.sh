#!/usr/bin/env bash
# Start or stop a reference Livestatus for conformance tests: a second Nagios with the
# Livestatus module of the site, next to the running core.
#
# The reference reads the object configuration and the log archive of the site. It
# runs no checks, sends no notifications, accepts no commands and keeps all of its
# own files (socket, log, status, lock) in WORKDIR.
#
# Usage (as the site user of an OMD site with the Nagios core installed):
#   reference.sh start WORKDIR
#   reference.sh stop WORKDIR
set -euo pipefail

die() {
    echo "reference.sh: $*" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: reference.sh start|stop WORKDIR"
[ -n "${OMD_ROOT:-}" ] || die "OMD_ROOT is not set, run this as the site user"
action=$1
work=$2
main=$OMD_ROOT/tmp/nagios/nagios.cfg

setting() {
    sed -n "s/^$1=//p" "$main" | tail -n 1
}

start() {
    [ -r "$main" ] || die "cannot read $main"
    [ ! -e "$work/nagios.lock" ] || die "$work already holds a running reference"
    mkdir -p "$work/archive" "$work/tmp" "$work/checkresults"
    work=$(cd "$work" && pwd)

    # The history of the site, read only: the archive and the current log file.
    archive=$(setting log_archive_path)
    for file in "$archive"/*; do
        [ -e "$file" ] && ln -sf "$file" "$work/archive/"
    done
    ln -sf "$(setting log_file)" "$work/archive/current.log"

    live=$(sed -n 's/^broker_module=.*livestatus\.o.* \([^ ]*\)$/\1/p' "$main" | tail -n 1)
    [ -n "$live" ] || die "no Livestatus broker module in $main"

    files='lock_file|temp_file|debug_file|command_file|log_file|object_cache_file|precached_object_file|status_file|state_retention_file'
    off='execute_service_checks|execute_host_checks|accept_passive_service_checks|accept_passive_host_checks'
    off="$off|enable_notifications|enable_event_handlers|process_performance_data|check_external_commands"
    off="$off|retain_state_information|check_service_freshness|check_host_freshness|enable_flap_detection"
    sed -E \
        -e "s#^($files)=.*/([^/]+)\$#\\1=$work/\\2#" \
        -e "s#^temp_path=.*#temp_path=$work/tmp#" \
        -e "s#^log_archive_path=.*#log_archive_path=$work/archive#" \
        -e "s#^check_result_path=.*#check_result_path=$work/checkresults#" \
        -e "s#^($off)=.*#\\1=0#" \
        -e "s#^log_rotation_method=.*#log_rotation_method=n#" \
        -e "/^broker_module=/{/livestatus\\.o/!d;}" \
        -e "s#^(broker_module=.*livestatus\\.o.* )$live\$#\\1$work/live#" \
        "$main" >"$work/nagios.cfg"

    # Refuse to start if anything would still be written outside of WORKDIR.
    paths="$files|temp_path|log_archive_path|check_result_path"
    if grep -E "^($paths)=" "$work/nagios.cfg" | grep -v "=$work/"; then
        die "the settings above still point outside of $work"
    fi
    grep -q "^broker_module=.* $work/live\$" "$work/nagios.cfg" || die "could not redirect the Livestatus socket"
    for name in ${off//|/ }; do
        grep -q "^$name=0\$" "$work/nagios.cfg" || echo "$name=0" >>"$work/nagios.cfg"
    done

    nagios -d "$work/nagios.cfg" >/dev/null
    for _ in $(seq 1 60); do
        if [ -S "$work/live" ] && printf 'GET status\nColumns: program_version\n\n' | unixcat "$work/live" >/dev/null 2>&1; then
            echo "reference Livestatus is listening on $work/live"
            return
        fi
        sleep 0.5
    done
    die "the reference did not start, see $work/nagios.log"
}

stop() {
    [ -f "$work/nagios.cfg" ] || die "$work is not a reference directory"
    if [ -f "$work/nagios.lock" ]; then
        pid=$(cat "$work/nagios.lock")
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 40); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.25
        done
    fi
    rm -rf "$work"
    echo "reference stopped, $work removed"
}

case $action in
    start) start ;;
    stop) stop ;;
    *) die "unknown action '$action'" ;;
esac
