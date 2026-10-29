#!/usr/bin/env bash
# Print the release version of a commit (default HEAD): its committer date in
# UTC and the first 12 characters of its hash, `YYYY.MM.DD-<sha12>`.
#
# This is the shell side of `teiryo_core::version::release_version`, the rule
# behind `-V`. `mise run release` names tags with it, release.yml checks tags
# against it, and CI checks every build's `-V` against it, so the two sides
# cannot drift apart without a pull request failing.
set -euo pipefail

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [<commit>]" >&2
  exit 2
fi

commit=$(git rev-parse --verify --quiet "${1:-HEAD}^{commit}") || {
  echo "release-version: ${1:-HEAD} is not a commit" >&2
  exit 1
}
date=$(TZ=UTC git log -1 --format=%cd --date=format-local:%Y.%m.%d "$commit")
echo "$date-$(printf %s "$commit" | cut -c1-12)"
