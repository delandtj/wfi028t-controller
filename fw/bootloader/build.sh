#!/usr/bin/env bash
# Build the rollback-enabled ESP32-C6 second-stage bootloader (ADR 0002).
#
# The result, esp32c6-bootloader-rollback.bin, is committed next to this
# script, so flashing the board does NOT need a container. Re-run this only
# when sdkconfig.defaults changes or when ESP-IDF moves on.
#
# What it does: writes a minimal ESP-IDF project into build/ (the bootloader
# does not care what the app is, only that there is a project to configure),
# runs `idf.py set-target esp32c6 bootloader` inside the pinned ESP-IDF
# container, and copies build/build/bootloader/bootloader.bin out.
#
# Requires podman (rootless is fine: container root maps to the invoking user,
# so the files it writes into build/ stay ours).
set -euo pipefail

IMAGE="docker.io/espressif/idf:release-v6.1"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT="$HERE/build"
OUTPUT="$HERE/esp32c6-bootloader-rollback.bin"

rm -rf "$PROJECT"
mkdir -p "$PROJECT/main"

# The smallest project ESP-IDF will configure. Only the bootloader is built
# from it; this app is never flashed (the Rust firmware is the app).
cat >"$PROJECT/CMakeLists.txt" <<'EOF'
cmake_minimum_required(VERSION 3.16)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(wfi028t_bootloader_host)
EOF

cat >"$PROJECT/main/CMakeLists.txt" <<'EOF'
idf_component_register(SRCS "main.c")
EOF

cat >"$PROJECT/main/main.c" <<'EOF'
void app_main(void) {}
EOF

cp "$HERE/sdkconfig.defaults" "$PROJECT/sdkconfig.defaults"

podman run --rm \
  -v "$PROJECT:/project:Z" \
  -w /project \
  "$IMAGE" \
  idf.py set-target esp32c6 bootloader

cp "$PROJECT/build/bootloader/bootloader.bin" "$OUTPUT"

echo
echo "wrote $OUTPUT"
sha256sum "$OUTPUT"
grep -h 'CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE\|CONFIG_ESPTOOLPY_FLASHSIZE=' \
  "$PROJECT/sdkconfig" || true
echo "IDF version: $(podman run --rm "$IMAGE" idf.py --version 2>/dev/null | tail -1)"
