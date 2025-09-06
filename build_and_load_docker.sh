#!/usr/bin/env bash

set -euo pipefail

echo "--- Building Docker image with Nix ---"
nix build .#oci-image --extra-experimental-features "nix-command flakes"

echo "--- Loading image into Docker ---"
docker load < ./result

echo "--- Cleaning up ---"
rm ./result

echo "--- Restarting Docker Compose setup ---"
docker-compose -f /home/deji/Documents/docker-stack/dockerfiles/matrix-server/docker-compose.yml down
docker-compose -f /home/deji/Documents/docker-stack/dockerfiles/matrix-server/docker-compose.yml up -d

echo "--- Done! ---"
echo "Image loaded into Docker and services restarted."