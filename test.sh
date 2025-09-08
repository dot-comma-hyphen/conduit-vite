#!/usr/bin/env bash

set -euo pipefail

echo "--- Running tests ---"
nix --extra-experimental-features "nix-command flakes" develop -c cargo test
if [ $? -ne 0 ]; then
    echo "Tests failed, aborting."
    exit 1
fi

echo "--- Tests passed! ---"
