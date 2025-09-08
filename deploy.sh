#!/usr/bin/env bash

set -euo pipefail

# Generate the dynamic tag
TAG="test"
IMAGE_NAME="ghcr.io/dot-comma-hyphen/conduit-vite"

echo "--- Tagging image as $IMAGE_NAME:$TAG ---"
docker tag conduit:next "$IMAGE_NAME:$TAG"

echo "--- Pushing image to GitHub Container Registry ---"
docker push "$IMAGE_NAME:$TAG"

echo "--- Done! ---"
echo "Image pushed successfully: $IMAGE_NAME:$TAG"
