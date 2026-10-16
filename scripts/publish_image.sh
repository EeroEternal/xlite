#!/usr/bin/env bash
# Build and push the xlite Docker image to GHCR and/or Docker Hub
# from a local Linux CI runner.
#
# Usage:
#   # 1. Authenticate once on the CI runner:
#   #    echo "$GHCR_TOKEN" | docker login ghcr.io -u eeroeternal --password-stdin
#   #    echo "$DOCKERHUB_TOKEN" | docker login -u <dockerhub-user> --password-stdin
#   # 2. Build & push current Cargo.toml version + :latest (linux/amd64 by default):
#   bash scripts/publish_image.sh
#   # 3. Also push to Docker Hub in the same run:
#   DOCKERHUB_IMAGE=eeroeternal/xlite bash scripts/publish_image.sh
#   # 4. Multi-arch build & push (linux/amd64,linux/arm64):
#   PLATFORMS=linux/amd64,linux/arm64 bash scripts/publish_image.sh
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${REGISTRY_IMAGE:-ghcr.io/eeroeternal/xlite}"
DOCKERHUB_IMAGE="${DOCKERHUB_IMAGE:-eeroeternal/xlite}"
VERSION="${VERSION:-$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml)}"
PLATFORMS="${PLATFORMS:-}"

if [[ -z "$VERSION" ]]; then
  echo "error: failed to read version from Cargo.toml" >&2
  exit 1
fi

TAGS=(-t "${IMAGE}:${VERSION}" -t "${IMAGE}:latest")
if [[ -n "$DOCKERHUB_IMAGE" ]]; then
  TAGS+=(-t "${DOCKERHUB_IMAGE}:${VERSION}" -t "${DOCKERHUB_IMAGE}:latest")
fi

echo "==> Publishing ${IMAGE}:${VERSION} and ${IMAGE}:latest${DOCKERHUB_IMAGE:+ (plus ${DOCKERHUB_IMAGE}:${VERSION} and ${DOCKERHUB_IMAGE}:latest)}"

if [[ -n "$PLATFORMS" ]]; then
  docker buildx create --name xlite-builder --use >/dev/null 2>&1 || docker buildx use xlite-builder
  docker buildx build \
    --platform "$PLATFORMS" \
    "${TAGS[@]}" \
    --push \
    .
else
  docker build \
    "${TAGS[@]}" \
    .
  docker push "${IMAGE}:${VERSION}"
  docker push "${IMAGE}:latest"
  if [[ -n "$DOCKERHUB_IMAGE" ]]; then
    docker push "${DOCKERHUB_IMAGE}:${VERSION}"
    docker push "${DOCKERHUB_IMAGE}:latest"
  fi
fi

echo "✓ Published ${IMAGE}:${VERSION} and ${IMAGE}:latest${DOCKERHUB_IMAGE:+ (and ${DOCKERHUB_IMAGE}:${VERSION}, ${DOCKERHUB_IMAGE}:latest)}"
