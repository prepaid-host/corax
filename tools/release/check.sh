#!/usr/bin/env bash
# Check that the version is consistent and print it:
# - the package version and the workspace dependencies in Cargo.toml agree
# - Cargo.lock has the same version for all corax crates
# - the newest release section of CHANGELOG.md is the one of this version
#
# Usage: check.sh            print the version
#        check.sh --notes    print the changelog section of the version instead
set -euo pipefail
cd "$(dirname "$0")/../.."

die() {
    echo "check.sh: $*" >&2
    exit 1
}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "no package version in Cargo.toml"

others=$(grep -E '^corax-[a-z]+ = \{' Cargo.toml | grep -vc "version = \"$version\"" || true)
[ "$others" = 0 ] || die "a workspace dependency in Cargo.toml does not have version $version"

locked=$(grep -A1 -E '^name = "corax(-[a-z]+)?"$' Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p' | sort -u)
[ "$locked" = "$version" ] || die "Cargo.lock has other versions of the corax crates: $(echo "$locked" | tr '\n' ' ')"

newest=$(sed -n 's/^## \[\([0-9][^]]*\)\] - [0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}$/\1/p' CHANGELOG.md | head -n 1)
[ "$newest" = "$version" ] || die "the newest section of CHANGELOG.md is [$newest], expected [$version]"

if [ "${1:-}" = "--notes" ]; then
    notes=$(awk -v section="## [$version]" '
        index($0, section) == 1 { found = 1; next }
        found && /^## \[/ { exit }
        found { print }
    ' CHANGELOG.md | sed -e '/./,$!d')
    [ -n "$notes" ] || die "the section [$version] of CHANGELOG.md is empty"
    printf '%s\n' "$notes"
else
    echo "$version"
fi
