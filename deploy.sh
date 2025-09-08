#!/usr/bin/env bash

set -euo pipefail

# Generate the dynamic tag
TAG="test-$(date +%Y%m%d-%H%M%S)"
IMAGE_NAME="ghcr.io/dot-comma-hyphen/conduit-vite"
FULL_IMAGE_NAME="$IMAGE_NAME:$TAG"

echo "--- Tagging image as $FULL_IMAGE_NAME ---"
docker tag conduit:next "$FULL_IMAGE_NAME"

echo "--- Pushing image to GitHub Container Registry ---"
docker push "$FULL_IMAGE_NAME"

echo "--- Restarting Matrix stack ---"
echo "Passing image to restart script: $FULL_IMAGE_NAME"
bash /home/deji/Documents/docker-stack/restart_matrix.sh "$FULL_IMAGE_NAME"

echo "--- Done! ---"
echo "Image pushed successfully: $FULL_IMAGE_NAME"
