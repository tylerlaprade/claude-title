#!/bin/sh
# Cut a release: tag the version in Cargo.toml, push, wait for CI to build
# and publish, then sync the Homebrew formula to the shared tap.
set -eu

version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
tag="v$version"

test -z "$(git status --porcelain)" || { echo "working tree not clean" >&2; exit 1; }
git tag "$tag"
git push origin master "$tag"

echo "waiting for release workflow on $tag..."
run=""
while [ -z "$run" ]; do
  sleep 10
  run=$(gh run list --workflow=release.yml --branch "$tag" --limit 1 --json databaseId -q '.[0].databaseId' 2>/dev/null || true)
done
gh run watch "$run" --exit-status

tap_dir=$(mktemp -d)
trap 'rm -rf "$tap_dir"' EXIT
gh release download "$tag" --pattern '*.rb' --dir "$tap_dir"
git clone --quiet --depth 1 git@github.com:tylerlaprade/homebrew-tap.git "$tap_dir/tap"
mkdir -p "$tap_dir/tap/Formula"
cp "$tap_dir"/*.rb "$tap_dir/tap/Formula/"
git -C "$tap_dir/tap" add Formula
if ! git -C "$tap_dir/tap" diff --cached --quiet; then
  git -C "$tap_dir/tap" commit --quiet -m "claude-title $version"
  git -C "$tap_dir/tap" push --quiet origin HEAD
  echo "formula synced to tap"
else
  echo "formula unchanged"
fi

echo "released $tag"
