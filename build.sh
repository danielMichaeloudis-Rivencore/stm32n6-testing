#!/usr/bin/env bash

set -e

cargo build "$@" 

if "$1"=="--release" (
    PROFILE=release
) else (
    PROFILE=debug
)

ELF="target\thumbv8m.main-none-eabihf\${PROFILE}\stm32n6-testing"

echo "Found ELF:"
echo "$ELF"

BIN="${ELF}.bin"

rust-objcopy \
    -O binary \
    "$ELF" \
    "$BIN"

echo "Generated:"
echo "$BIN"

OUT="${ELF}-Signed.bin"

STM32_SigningTool_CLI \
    -bin "${BIN}" \
    -nk \
    -of 0x80000000 \
    -t fsbl \
    -o "${OUT}" \
    -hv 2.3 \
    -dump "${OUT}" \
    -align \
    -s

echo "Signed:"
echo "${OUT}

echo "Build Complete"