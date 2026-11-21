#!/usr/bin/env bash
# Linux CI: the kernel's simulated SMBus chip (the i2c-stub module) at the IT8915's address
# 0x2B, holding known pin and tilt values, so the real I2C_SMBUS ioctls in
# rust/src/sensor/i2cdev.rs run against a kernel driver (its i2c_stub_* tests). The i2cset
# writes below go only to this simulated chip's memory, on a CI machine with no GPU: the
# program itself never writes to I2C (safety rule 1). Prints the stub's bus number.
set -euo pipefail
sudo modprobe i2c-dev
if ! sudo modprobe i2c-stub chip_addr=0x2b 2>/dev/null; then
    # Some kernels ship it in the extra modules.
    sudo apt-get -o Acquire::Retries=5 install -y --no-install-recommends "linux-modules-extra-$(uname -r)" >&2
    sudo modprobe i2c-stub chip_addr=0x2b
fi
bus=""
for a in /sys/class/i2c-adapter/i2c-*; do
    if [ "$(cat "$a/name")" = "SMBus stub driver" ]; then bus=${a##*/i2c-}; fi
done
[ -n "$bus" ] || { echo "::error::i2c-stub loaded but no 'SMBus stub driver' adapter" >&2; exit 1; }

put() { sudo i2cset -y "$bus" 0x2b "$(printf '0x%02x' "$1")" "$(printf '0x%02x' "$2")" b; }
# Pins from 0x80, in reverse order (pin 6 first): big-endian millivolts, then milliamps.
# Pin p: 12000 + 10 p mV and 1000 p + 500 mA.
for p in 1 2 3 4 5 6; do
    base=$((0x80 + (6 - p) * 4)) mv=$((12000 + 10 * p)) ma=$((1000 * p + 500))
    put "$base" $((mv >> 8)); put $((base + 1)) $((mv & 255))
    put $((base + 2)) $((ma >> 8)); put $((base + 3)) $((ma & 255))
done
# Tilt from 0x70: x, y, z in signed big-endian 1/4096 g and one more byte; lying flat, 1 g.
reg=$((0x70))
for b in 0 0 0 0 16 0 0; do put "$reg" "$b"; reg=$((reg + 1)); done

sudo chmod a+rw "/dev/i2c-$bus"
echo "$bus"
