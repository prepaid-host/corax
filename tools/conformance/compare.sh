#!/usr/bin/env bash
# Run every query in QUERY_DIR against the reference Livestatus and against corax,
# and compare the rows. Queries must use `OutputFormat: json`. Neither the order of the
# rows nor the order of the keys in dictionary columns is compared.
#
# Usage: compare.sh WORKDIR QUERY_DIR
#   WORKDIR    directory of a reference started with reference.sh
#   CORAX      corax binary to test (default: corax)
#
# Exit code 0 if all answers are equal. The answers are kept in WORKDIR/out.
set -euo pipefail

[ $# -eq 2 ] || {
    echo "usage: compare.sh WORKDIR QUERY_DIR" >&2
    exit 1
}
work=$1
queries=$2
corax=${CORAX:-corax}
out=$work/out
[ -S "$work/live" ] || {
    echo "compare.sh: no reference socket at $work/live" >&2
    exit 1
}
mkdir -p "$out"

# One row per line, sorted, with the keys of dictionaries in alphabetical order (the
# reference emits them in the order of a hash table). An answer that is not JSON, such
# as an error message, is kept as it is.
rows() {
    python3 - "$1" <<'PYTHON'
import json
import sys

with open(sys.argv[1], encoding="utf-8", errors="replace") as answer:
    text = answer.read()
try:
    lines = sorted(json.dumps(row, sort_keys=True, ensure_ascii=False) for row in json.loads(text))
except ValueError:
    lines = text.splitlines()
print("\n".join(lines))
PYTHON
}

now() {
    date +%s.%N
}

failed=0
printf '%-28s %9s %9s %9s %9s %9s\n' query rows differing reference corax result
for file in "$queries"/*.lql; do
    name=$(basename "$file" .lql)
    start=$(now)
    { cat "$file"; echo; } | unixcat "$work/live" >"$out/$name.reference"
    middle=$(now)
    "$corax" --query "$file" "$work/nagios.cfg" >"$out/$name.corax" || true
    end=$(now)
    rows "$out/$name.reference" >"$out/$name.reference.rows"
    rows "$out/$name.corax" >"$out/$name.corax.rows"
    differing=$(diff -a "$out/$name.reference.rows" "$out/$name.corax.rows" | grep -ac '^[<>]' || true)
    result=equal
    if [ "$differing" != 0 ]; then
        result=DIFFERENT
        failed=1
    fi
    printf '%-28s %9s %9s %8.2fs %8.2fs %9s\n' "$name" "$(wc -l <"$out/$name.reference.rows")" "$differing" \
        "$(echo "$middle - $start" | bc)" "$(echo "$end - $middle" | bc)" "$result"
done
[ "$failed" = 0 ] || echo "see $out for the answers (*.rows are normalized)" >&2
exit "$failed"
