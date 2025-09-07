#!/usr/bin/env bash

set -euo pipefail

echo "--- Running tests ---"
nix --extra-experimental-features "nix-command flakes" develop -c cargo test
if [ $? -ne 0 ]; then
    echo "Tests failed, aborting build."
    exit 1
fi

echo "--- Building Docker image with Nix ---"
nix build .#oci-image --extra-experimental-features "nix-command flakes"

echo "--- Loading image into Docker ---"
docker load < ./result

# Generate the dynamic tag
TAG="test"
IMAGE_NAME="ghcr.io/dot-comma-hyphen/conduit-vite"

echo "--- Tagging image as $IMAGE_NAME:$TAG ---"
docker tag conduit:next "$IMAGE_NAME:$TAG"

echo "--- Pushing image to GitHub Container Registry ---"
docker push "$IMAGE_NAME:$TAG"

echo "--- Cleaning up ---"
rm ./result

echo "--- Done! ---"
echo "Image pushed successfully: $IMAGE_NAME:$TAG"
