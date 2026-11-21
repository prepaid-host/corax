#!/usr/bin/env bash
# Prepare a release: set the version everywhere, turn the "Unreleased" section of the
# changelog into the section of the release, and open the release pull request.
#
# Merging that pull request publishes the release (see .github/workflows/release.yml).
#
# Usage: prepare.sh VERSION      e.g. prepare.sh 0.6.0
set -euo pipefail
cd "$(dirname "$0")/../.."

die() {
    echo "prepare.sh: $*" >&2
    exit 1
}

[ $# -eq 1 ] || die "usage: prepare.sh VERSION"
new=$1
[[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'$new' is not a version like 1.2.3"
old=$(tools/release/check.sh)
[ "$old" != "$new" ] || die "the version is already $new"
[ -z "$(git status --porcelain)" ] || die "the working tree has uncommitted changes"
[ "$(git branch --show-current)" = main ] || die "releases are prepared from the main branch"
git fetch --quiet origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main is not up to date with origin"

unreleased=$(awk '/^## \[Unreleased\]/ { found = 1; next } found && /^## \[/ { exit } found { print }' CHANGELOG.md)
[ -n "$(printf '%s' "$unreleased" | tr -d '[:space:]')" ] || die "the Unreleased section of CHANGELOG.md is empty"

git checkout --quiet -b "release-$new"
today=$(date +%Y-%m-%d)
OLD=$old NEW=$new perl -0pi -e 's/version = "\Q$ENV{OLD}\E"/version = "$ENV{NEW}"/g' Cargo.toml
OLD=$old NEW=$new perl -0pi -e 's/(name = "corax(?:-[a-z]+)?"\nversion = )"\Q$ENV{OLD}\E"/$1"$ENV{NEW}"/g' Cargo.lock
NEW=$new TODAY=$today perl -0pi -e 's/## \[Unreleased\]\n/## [Unreleased]\n\n## [$ENV{NEW}] - $ENV{TODAY}\n/' CHANGELOG.md

[ "$(tools/release/check.sh)" = "$new" ] || die "the version is not consistent after the change"
git commit --quiet --all --message "Release $new"
echo "created branch release-$new with the commit 'Release $new'"

if command -v gh >/dev/null; then
    git push --quiet --set-upstream origin "release-$new"
    gh pr create --base main --head "release-$new" --title "Release $new" --body "$(
        printf 'Version %s. Merging this pull request publishes the release.\n\n' "$new"
        tools/release/check.sh --notes
    )"
else
    echo "push the branch and open a pull request to main; merging it publishes the release"
fi
