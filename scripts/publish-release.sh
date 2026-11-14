#!/usr/bin/env bash
# Independent OS workflows add validated installers to the same commit release.
set -euo pipefail
: "${VERSION:?}" "${GITHUB_REPOSITORY:?}" "${GITHUB_SHA:?}"
commit="${RELEASE_COMMIT:-$GITHUB_SHA}"
tag="v$VERSION"
# A draft release does not create its tag until publication. Create the ref
# first, so commit verification also works for the first OS to publish.
if ! gh api "repos/$GITHUB_REPOSITORY/git/ref/tags/$tag" --silent >/dev/null 2>&1; then
  gh api --method POST "repos/$GITHUB_REPOSITORY/git/refs" \
    -f "ref=refs/tags/$tag" -f "sha=$commit" --silent ||
    gh api "repos/$GITHUB_REPOSITORY/git/ref/tags/$tag" --silent
fi
# Creation can race with another OS. Only tolerate that error if the release
# now exists, and verify the tag belongs to this commit before uploading.
if ! gh release view "$tag" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  gh release create "$tag" --repo "$GITHUB_REPOSITORY" \
    --target "$commit" --title "Pulpo $tag" --generate-notes --draft ||
    gh release view "$tag" --repo "$GITHUB_REPOSITORY" >/dev/null
fi
actual=$(gh api "repos/$GITHUB_REPOSITORY/commits/$tag" --jq .sha)
if [ "$actual" != "$commit" ]; then
  echo "::error::Release $tag points to $actual instead of $commit"
  exit 1
fi
gh release upload "$tag" "$@" --clobber --repo "$GITHUB_REPOSITORY"
# Each OS publishes immediately after its own checks; no global build gate.
# GitHub chooses latest by version/creation date, so a slow older build cannot
# explicitly promote itself over a newer release.
gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false
