#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/release.sh [--push] [--dry-run]

Create the timestamp/hash release tag for the current main commit.

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
git fetch --quiet "$remote" "$branch:refs/remotes/$remote/$branch" --tags

head_commit=$(git rev-parse HEAD)
remote_commit=$(git rev-parse "$remote/$branch")
if [[ "$head_commit" != "$remote_commit" ]]; then
  die "local $branch ($head_commit) does not match $remote/$branch ($remote_commit)"
fi

tag=$(scripts/release-tag.sh)

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  tagged_commit=$(git rev-list -n 1 "$tag")
  die "local tag $tag already exists at $tagged_commit"
fi

if git ls-remote --exit-code --tags "$remote" "refs/tags/$tag" >/dev/null 2>&1; then
  die "$remote already has tag $tag"
fi

if command -v gh >/dev/null 2>&1 && gh release view "$tag" >/dev/null 2>&1; then
  die "GitHub release $tag already exists"
fi

if [[ "$dry_run" == 1 ]]; then
  printf 'release: tag %s\n' "$tag"
  printf 'release: would create local tag %s at %s\n' "$tag" "$head_commit"
  if [[ "$push" == 1 ]]; then
    printf 'release: would push %s and refs/tags/%s to %s\n' "$branch" "$tag" "$remote"
  fi
  exit 0
fi

# Use a lightweight tag on purpose. shdeps release identity embeds the target
# commit suffix in the tag itself; annotated tags add a separate tag-object hash
# that provides no value here and can confuse CI environments that expose the
# triggering SHA differently for annotated tag pushes.
git tag "$tag"
created_tag=1
pushed_tag=0
cleanup_unpushed_tag() {
  if [[ "$push" == 1 && "$created_tag" == 1 && "$pushed_tag" == 0 ]]; then
    git tag -d "$tag" >/dev/null 2>&1 || true
  fi
}
trap cleanup_unpushed_tag EXIT

printf 'release: created tag %s\n' "$tag"

if [[ "$push" == 1 ]]; then
  git push --quiet "$remote" "$branch"
  git push --quiet "$remote" "refs/tags/$tag"
  pushed_tag=1
  printf 'release: pushed tag %s\n' "$tag"
else
  printf 'release: tag %s is local only; rerun with --push to publish\n' "$tag"
fi

printf 'release: %s\n' "$tag"
