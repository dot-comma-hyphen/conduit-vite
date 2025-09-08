#!/usr/bin/env bash

set -euo pipefail

echo "--- Building Docker image with Nix ---"
nix build .#oci-image --extra-experimental-features "nix-command flakes"

echo "--- Loading image into Docker ---"
docker load < ./result

echo "--- Cleaning up ---"
rm ./result

echo "--- Done! ---"
echo "Image built and loaded successfully."
