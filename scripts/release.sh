#!/usr/bin/env bash
# Tag origin/master's head as a signed release and push the tag
# (`mise run release`); with --dry-run, run every check and print the version
# that would be released, without tagging (`mise run release:dry`).
#
# A release is a signed annotated tag named by the commit's own version,
# `YYYY.MM.DD-<sha12>` (scripts/release-version.sh), the rule
# `teiryo_core::version::release_version` applies to `-V`. Pushing the tag is
# the whole release: .github/workflows/release.yml checks the signature,
# builds, and checks every binary's `-V` against the tag before it publishes
# anything.
#
# Only a commit already on origin/master whose CI passed is released, from a
# clean tree, so the tag names exactly what everyone else can fetch and what CI
# already built on every release target. Signing uses git's own configuration
# (`user.signingkey`, `gpg.format`); the key must be registered on GitHub as a
# signing key, or the workflow refuses the tag as unverified.
set -euo pipefail

dry_run=false
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=true ;;
  *)
    echo "usage: $0 [--dry-run]" >&2
    exit 2
    ;;
esac

fail() {
  echo "release: $*" >&2
  exit 1
}

cd "$(git rev-parse --show-toplevel)"

# Untracked files are not counted: the tag names a commit, which they cannot
# change, and editor or agent state (`.claude/`) would otherwise block it.
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  fail "the working tree has uncommitted changes"
fi
branch=$(git symbolic-ref -q --short HEAD || true)
if [ "$branch" != master ]; then
  fail "releases are cut from master, not ${branch:-a detached HEAD}"
fi
git fetch --quiet origin master
head=$(git rev-parse HEAD)
if [ "$head" != "$(git rev-parse origin/master)" ]; then
  fail "master is not level with origin/master; push or pull first"
fi

version=$(scripts/release-version.sh "$head")
if git rev-parse -q --verify "refs/tags/$version" >/dev/null; then
  fail "tag $version already exists"
fi
if [ -n "$(git ls-remote --tags origin "refs/tags/$version")" ]; then
  fail "tag $version already exists on origin"
fi

# CI on master runs the release build on every target (ci.yml), so a commit
# it passed is one the release workflow can build. Anything short of a
# finished, successful run is refused: a failure, a run still going, and no
# run at all, which is what a commit pushed moments ago has.
command -v gh >/dev/null 2>&1 || fail "gh is required to check this commit's CI"
ci=$(gh run list --workflow ci.yml --commit "$head" --event push --limit 1 \
  --json status,conclusion,url \
  --jq '.[0] // empty | "\(.status)|\(.conclusion)|\(.url)"') ||
  fail "could not read CI runs for $head"
if [ -z "$ci" ]; then
  fail "CI has not run on $head yet"
fi
# `|`, not a tab: whitespace separators collapse, and a run still going has an
# empty conclusion that would shift the URL into its place.
IFS='|' read -r status conclusion url <<<"$ci"
if [ "$status" != completed ]; then
  fail "CI on $head is still $status: $url"
fi
if [ "$conclusion" != success ]; then
  fail "CI on $head concluded $conclusion: $url"
fi

if "$dry_run"; then
  echo "release: would tag and push $version ($head); CI passed: $url"
  exit 0
fi

# Whether the signature is good is GitHub's call, made by the workflow against
# the keys registered there; `git tag -v` would need an allowed-signers file
# for SSH keys. What is checked here is that signing happened at all, so an
# unsigned tag is never pushed.
git tag -s "$version" -m "Teiryo $version" "$head"
if ! git cat-file tag "$version" | grep -q "^-----BEGIN "; then
  git tag -d "$version" >/dev/null
  fail "the new tag carries no signature; it was deleted"
fi
# A tag left behind by a failed push would make every rerun refuse it as
# existing, so it goes: the rerun tags again.
if ! git push origin "refs/tags/$version"; then
  git tag -d "$version" >/dev/null
  fail "pushing $version failed; the local tag was deleted"
fi
echo "release: pushed $version; the release workflow publishes it"
echo "release: follow it with: gh run list --workflow release.yml --limit 1"
