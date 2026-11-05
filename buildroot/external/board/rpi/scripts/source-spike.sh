#!/bin/sh
# Hardware spike for Pi mode (design note S1). Run on the Pi as root.
# Prints what the image offers for the frame-capture routes in design note 3.3.
# Paste the whole output into the PR as a comment.

sec() { printf '\n=== %s ===\n' "$1"; }

sec "Image and kernel"
uname -a
cat /etc/issue 2>/dev/null
grep -E 'Model|Revision' /proc/cpuinfo

sec "Boot time (uptime, boot log)"
cat /proc/uptime
dmesg 2>/dev/null | grep -iE 'Run /sbin/init|Freeing unused' | head -3
if [ -f /var/log/aa-proxy-rs.log ]; then
    echo "-- first lines of aa-proxy-rs.log"
    head -3 /var/log/aa-proxy-rs.log
fi

sec "Free memory"
free -m 2>/dev/null || cat /proc/meminfo | head -5
grep -E 'CmaTotal|CmaFree' /proc/meminfo

sec "GPU memory (firmware)"
vcgencmd get_mem gpu 2>/dev/null || echo "vcgencmd not installed"
cat /proc/device-tree/chosen/bootargs 2>/dev/null; echo

sec "/dev/video* (V4L2 codec nodes: expect an H.264 encoder, usually /dev/video11)"
ls -l /dev/video* 2>&1
if command -v v4l2-ctl >/dev/null 2>&1; then
    for d in /dev/video*; do
        [ -e "$d" ] || continue
        echo "--- $d"
        v4l2-ctl -d "$d" --info 2>&1 | grep -E 'Driver name|Card type|Device Caps|Video .*Mem|Memory-to-Memory|Capture|Output'
        v4l2-ctl -d "$d" --list-formats-out 2>&1 | grep -E "'[A-Z0-9]{4}'"
        v4l2-ctl -d "$d" --list-formats 2>&1 | grep -E "'[A-Z0-9]{4}'"
    done
else
    echo "v4l2-ctl not installed"
fi

sec "/dev/dri (vc4 KMS)"
ls -l /dev/dri 2>&1
sec "modetest -c (look for a Writeback connector)"
if command -v modetest >/dev/null 2>&1; then
    modetest -c 2>&1
    echo "-- writeback connectors:"
    modetest -c 2>&1 | grep -i writeback || echo "NONE"
else
    echo "modetest not installed"
fi
sec "DRM sysfs connectors"
ls /sys/class/drm 2>&1

sec "/dev/uinput and evdev"
ls -l /dev/uinput /dev/input 2>&1

sec "aplay -l (look for a Loopback card)"
if command -v aplay >/dev/null 2>&1; then
    aplay -l 2>&1
    aplay -l 2>&1 | grep -i loopback || echo "Loopback card: NONE"
else
    echo "aplay not installed"
fi
cat /proc/asound/cards 2>&1

sec "Related kernel messages"
dmesg 2>/dev/null | grep -iE 'bcm2835-codec|vchiq|vc4|drm|aloop|loopback|uinput' | head -40

sec "Done"
