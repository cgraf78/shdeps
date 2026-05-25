#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/release.sh [--push] [--dry-run]

Create the UTC commit-timestamp/hash release tag for the current main commit.

Options:
  --push     push main and the generated tag to origin
  --dry-run  validate and print the tag/actions without creating anything
EOF
}

push=0
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --push)
      push=1
      ;;
    --dry-run)
      dry_run=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
  shift
done

remote=${SHDEPS_RELEASE_REMOTE:-origin}
branch=${SHDEPS_RELEASE_BRANCH:-main}
remote_ref="refs/remotes/$remote/$branch"

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

die() {
  printf 'release: %s\n' "$*" >&2
  exit 1
}

if [[ $(git branch --show-current) != "$branch" ]]; then
  die "run from $branch"
fi

if [[ -n $(git status --porcelain) ]]; then
  die "worktree must be clean"
fi

# Fetch before deriving the tag so local release cuts are anchored to the same
# commit that GitHub will build. The explicit refspec keeps this independent of
# whatever upstream tracking configuration a developer happens to have locally.
git fetch --quiet --tags "$remote" "+refs/heads/$branch:$remote_ref"

head_commit=$(git rev-parse HEAD)
remote_commit=$(git rev-parse "$remote_ref")
if [[ "$head_commit" != "$remote_commit" ]]; then
  die "local $branch ($head_commit) does not match $remote/$branch ($remote_commit)"
fi

# `release-version.sh` intentionally honors several CI/build environment
# overrides because packaging jobs and source snapshots need that flexibility.
# The release cutter has a narrower contract: publish the clean local HEAD that
# was just verified against origin/main. Keep ambient shell/GitHub env from
# changing which tag this script creates.
tag=$(
  unset SHDEPS_BUILD_VERSION SHDEPS_BUILD_TIMESTAMP GITHUB_REF_TYPE GITHUB_REF_NAME GITHUB_REF GITHUB_SHA
  SHDEPS_BUILD_COMMIT="$head_commit" scripts/release-tag.sh
)
tag_exists=0

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  tag_type=$(git cat-file -t "refs/tags/$tag")
  if [[ "$tag_type" != "commit" ]]; then
    die "local tag $tag is a $tag_type object; release tags must be lightweight"
  fi
  tagged_commit=$(git rev-list -n 1 "$tag")
  if [[ "$tagged_commit" != "$head_commit" ]]; then
    die "local tag $tag already points at $tagged_commit, not HEAD $head_commit"
  fi
  tag_exists=1
fi

if git ls-remote --exit-code --tags "$remote" "refs/tags/$tag" >/dev/null 2>&1; then
  die "$remote already has tag $tag"
fi

if command -v gh >/dev/null 2>&1 && gh release view "$tag" >/dev/null 2>&1; then
  die "GitHub release $tag already exists"
fi

if [[ "$dry_run" == 1 ]]; then
  printf 'release: tag %s\n' "$tag"
  if [[ "$tag_exists" == 1 ]]; then
    printf 'release: local tag %s already exists at HEAD\n' "$tag"
  else
    printf 'release: would create local tag %s at %s\n' "$tag" "$head_commit"
  fi
  if [[ "$push" == 1 ]]; then
    printf 'release: would push %s and refs/tags/%s to %s\n' "$branch" "$tag" "$remote"
  fi
  exit 0
fi

# Use a lightweight tag on purpose. shdeps release identity embeds the target
# commit suffix in the tag itself; annotated tags add a separate tag-object hash
# that provides no value here and can confuse CI environments that expose the
# triggering SHA differently for annotated tag pushes.
created_tag=1
pushed_tag=0
cleanup_unpushed_tag() {
  if [[ "$push" == 1 && "$created_tag" == 1 && "$pushed_tag" == 0 ]]; then
    git tag -d "$tag" >/dev/null 2>&1 || true
  fi
}
trap cleanup_unpushed_tag EXIT

if [[ "$tag_exists" == 1 ]]; then
  created_tag=0
  printf 'release: local tag %s already exists at HEAD\n' "$tag"
else
  git tag "$tag"
  printf 'release: created tag %s\n' "$tag"
fi

if [[ "$push" == 1 ]]; then
  git push --quiet "$remote" "$branch"
  git push --quiet "$remote" "refs/tags/$tag"
  pushed_tag=1
  printf 'release: pushed tag %s\n' "$tag"
else
  printf 'release: tag %s is local only; rerun with --push to publish\n' "$tag"
fi

printf 'release: %s\n' "$tag"
