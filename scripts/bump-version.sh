#!/usr/bin/env bash
# Release mtr-ng: bump the version, run the CI gate, commit, tag, and push the
# commit and the tag together. The tag starts .github/workflows/release.yml
# (dist). Everything before the push is local and can be undone.
#
#   scripts/bump-version.sh 0.4.0
#   REMOTE=upstream scripts/bump-version.sh 0.4.0

set -euo pipefail

REMOTE=${REMOTE:-origin}
die() { echo "error: $*" >&2; exit 1; }

[[ $# -eq 1 ]] || die "usage: $0 X.Y.Z"
VERSION=$1
TAG="v$VERSION"
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "not a semantic version: $VERSION"

cd "$(git rev-parse --show-toplevel)"
[[ -z $(git status --porcelain) ]] || die "the working tree has uncommitted changes"
[[ $(git branch --show-current) == master ]] || die "releases are cut from master"
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null && die "$TAG already exists"

CURRENT=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "mtr-ng $CURRENT -> $VERSION"

# The first `version =` line is the package's own.
sed -i.bak "1,/^version = /s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
rm Cargo.toml.bak
cargo update --workspace --quiet          # the package's entry in Cargo.lock
cargo run --quiet --example man > install/mtr-ng.1

cargo fmt --check
cargo clippy --all-targets --locked --quiet -- -D warnings
cargo test --locked --quiet
if command -v dist >/dev/null; then
    dist plan --tag "$TAG" >/dev/null     # fails if dist would reject the tag
fi

git commit --quiet -m "Release $VERSION" -- Cargo.toml Cargo.lock install/mtr-ng.1
git tag -a "$TAG" -m "mtr-ng $VERSION"
echo "Committed and tagged $TAG."

read -r -p "Push master and $TAG to $REMOTE, starting the release? [y/N] " answer
if [[ $answer != [yY] ]]; then
    echo "Not pushed. Later: git push --atomic $REMOTE master $TAG"
    echo "Undo:  git tag -d $TAG && git reset --hard HEAD~1"
    exit 0
fi
git push --atomic "$REMOTE" master "$TAG"
url=$(git remote get-url "$REMOTE" | sed -E 's#(git@|https://)github.com[:/]#https://github.com/#; s#\.git$##')
echo "Release running: $url/actions"
