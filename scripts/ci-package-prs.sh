#!/bin/sh
# CI only: after a release is published, opens a pull request on the Homebrew tap and on the Scoop
# bucket that points them at it. The owner reviews and merges them; nothing here merges.
#
#   scripts/ci-package-prs.sh VERSION DIR
#
# DIR holds the release's rodu.rb and rodu.json. gh must be signed in with write access to the
# contents and pull requests of both repositories, and git able to push to them.
set -eu

version=$1
dir=$(cd "$2" && pwd)
owner=${PACKAGES_OWNER:-thedevper}
remote=${PACKAGES_REMOTE:-https://github.com/$owner}
branch="rodu-$version"
work=$(mktemp -d)

# repository, file in the release, path in the repository, command that upgrades
open_pr() {
  repo=$1 file=$2 path=$3 upgrade=$4
  # Re-running the job must not open a second pull request, nor reopen one the owner closed.
  existing=$(gh pr list --repo "$owner/$repo" --head "$branch" --state all --json url --jq '.[].url')
  if [ -n "$existing" ]; then
    echo "$owner/$repo: $existing already proposes $version"
    return
  fi
  git clone --quiet --depth 1 "$remote/$repo.git" "$work/$repo"
  cp "$dir/$file" "$work/$repo/$path"
  if git -C "$work/$repo" diff --quiet; then
    echo "$owner/$repo: already at $version"
    return
  fi
  git -C "$work/$repo" switch --quiet -c "$branch"
  git -C "$work/$repo" add "$path"
  git -C "$work/$repo" commit --quiet -s -m "rodu $version" \
    -m "From https://github.com/$owner/rodu/releases/tag/v$version"
  git -C "$work/$repo" push --quiet origin "$branch"
  gh pr create --repo "$owner/$repo" --head "$branch" --title "rodu $version" --body "Points \`$path\` at \
https://github.com/$owner/rodu/releases/tag/v$version. CI copied \`$file\` from the release, where \
\`cargo xtask release-files\` wrote it from the archives' hashes. Once merged, \`$upgrade\` installs $version."
}

open_pr homebrew-tap rodu.rb Formula/rodu.rb "brew upgrade rodu"
open_pr scoop-bucket rodu.json bucket/rodu.json "scoop update rodu"
