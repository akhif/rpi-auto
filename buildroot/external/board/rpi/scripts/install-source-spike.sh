#!/bin/bash
# Post-build hook (rpi02w only): install the Pi mode hardware spike into the rootfs.
set -eu
install -D -m 0755 "$(dirname "$0")/source-spike.sh" "${TARGET_DIR}/usr/bin/source-spike.sh"
