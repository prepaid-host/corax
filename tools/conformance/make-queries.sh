#!/usr/bin/env bash
# Write the standard set of conformance queries for a site into a directory.
#
# Usage: make-queries.sh SINCE UNTIL HOST DIR
#   SINCE, UNTIL  Unix times of a period that lies completely in the past
#   HOST          name of a host with some history in that period
set -euo pipefail

[ $# -eq 4 ] || {
    echo "usage: make-queries.sh SINCE UNTIL HOST DIR" >&2
    exit 1
}
since=$1
until=$2
host=$3
dir=$4
mid=$((since + (until - since) / 3 + 4567))
mkdir -p "$dir"

query() {
    name=$1
    shift
    {
        printf '%s\n' "$@"
        printf 'OutputFormat: json\n'
    } >"$dir/$name.lql"
}

period="Filter: time >= $since"$'\n'"Filter: time < $until"
history='host_name service_description time lineno from until duration duration_part state host_down in_downtime'
history="$history in_host_downtime is_flapping in_notification_period in_service_period notification_period"
history="$history service_period debug_info log_output long_log_output duration_ok duration_part_warning"
history="$history duration_unmonitored"

# state history
query statehist-hosts "GET statehist" "Columns: $history" "$period" "Filter: service_description = "
query statehist-one-host "GET statehist" "Columns: $history" "$period" "Filter: host_name = $host"
query statehist-all "GET statehist" "Columns: $history" "Filter: time >= $mid" "Filter: time < $until"
query statehist-stats "GET statehist" "Columns: host_name service_description state" "$period" \
    "Stats: sum duration" "Stats: sum duration_part" "Stats: sum duration_ok"
query statehist-problems "GET statehist" "Columns: $history" "$period" "Filter: state != 0" \
    "Filter: in_downtime = 1" "Or: 2"
query statehist-current "GET statehist" \
    "Columns: host_name service_description from until state current_host_alias current_service_check_command" \
    "$period" "Filter: current_host_name = $host" "Filter: current_service_description ~ ."
query statehist-no-lower-bound "GET statehist" "Columns: host_name service_description from until state" \
    "Filter: time < $since" "Filter: host_name = $host"

# log table
log='time lineno class type message options state state_type attempt host_name service_description'
log="$log contact_name command_name plugin_output long_plugin_output comment state_info"
query log-alerts "GET log" "Columns: $log" "$period" "Filter: class = 1"
query log-notifications "GET log" "Columns: $log" "$period" "Filter: class = 3"
query log-one-host "GET log" "Columns: $log" "$period" "Filter: host_name = $host"
query log-stats "GET log" "Columns: class type" "$period" "Stats: state >= 0"

# configuration (the state of the reference is not that of the site)
query hosts "GET hosts" \
    "Columns: name alias address groups contacts contact_groups parents childs check_command check_interval retry_interval max_check_attempts notification_period check_period custom_variables tags labels num_services"
query services "GET services" \
    "Columns: host_name description groups contacts contact_groups check_command check_interval retry_interval max_check_attempts notification_period check_period custom_variables" \
    "Filter: host_name = $host"
query hostgroups "GET hostgroups" "Columns: name alias members num_hosts"
query servicegroups "GET servicegroups" "Columns: name alias num_services"
query contacts "GET contacts" \
    "Columns: name alias email host_notification_period service_notification_period custom_variables"
query contactgroups "GET contactgroups" "Columns: name alias members"
query commands "GET commands" "Columns: name line"
query timeperiods "GET timeperiods" "Columns: name alias"

echo "wrote $(find "$dir" -name '*.lql' | wc -l) queries to $dir"
