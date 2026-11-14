#!/usr/bin/env bash
# Independent OS workflows add validated installers to the same commit release.
set -euo pipefail
: "${VERSION:?}" "${GITHUB_REPOSITORY:?}" "${GITHUB_SHA:?}"
tag="v$VERSION"
# Creation can race with another OS. Only tolerate that error if the release
# now exists, and verify the tag belongs to this commit before uploading.
if ! gh release view "$tag" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  gh release create "$tag" --repo "$GITHUB_REPOSITORY" \
    --target "$GITHUB_SHA" --title "Pulpo $tag" --generate-notes --draft ||
    gh release view "$tag" --repo "$GITHUB_REPOSITORY" >/dev/null
fi
actual=$(gh api "repos/$GITHUB_REPOSITORY/commits/$tag" --jq .sha)
if [ "$actual" != "$GITHUB_SHA" ]; then
  echo "::error::Release $tag points to $actual instead of $GITHUB_SHA"
  exit 1
fi
gh release upload "$tag" "$@" --clobber --repo "$GITHUB_REPOSITORY"
# Each OS publishes immediately after its own checks; no global build gate.
# GitHub chooses latest by version/creation date, so a slow older build cannot
# explicitly promote itself over a newer release.
gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false
