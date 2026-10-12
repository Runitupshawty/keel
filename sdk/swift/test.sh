#!/bin/sh
set -eu
cd "$(dirname "$0")"
output=$(mktemp -d)
trap 'rm -rf "$output"' EXIT HUP INT TERM
swiftc -swift-version 6 -warnings-as-errors -module-cache-path "$output/modules" \
    Keel.swift client_tests.swift -o "$output/client-tests"
"$output/client-tests"
