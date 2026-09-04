#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "Release requires a clean working tree." >&2
  exit 1
fi

read -r -p "Next release version (for example 0.4.0): " VERSION
VERSION="${VERSION#v}"
if [[ ! "${VERSION}" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid semantic version: ${VERSION}" >&2
  exit 1
fi

TAG="v${VERSION}"
if git rev-parse --verify --quiet "refs/tags/${TAG}" >/dev/null; then
  echo "Tag already exists: ${TAG}" >&2
  exit 1
fi

export RELEASE_VERSION="${VERSION}"
sed -i -E 's/^version = "[^"]+"/version = "'"${VERSION}"'"/' Cargo.toml
sed -i -E "s/^pkgver=.*/pkgver=${VERSION}/" packaging/arch/PKGBUILD
cargo check
git add Cargo.toml Cargo.lock packaging/arch/PKGBUILD
git commit -m "release: ${TAG}"
git tag -a "${TAG}" -m "Release ${TAG}"
git push origin HEAD "${TAG}"
echo "Pushed ${TAG}; GitHub Actions will publish the release artifacts."
