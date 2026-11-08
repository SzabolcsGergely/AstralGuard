#!/bin/sh
# Install AstralGuard on Linux: the protection service as a root systemd unit, the tray/
# dashboard as an autostart entry for the desktop user. From the release tarball, or from
# the source tree after `cargo build --release`, as root:  sudo ./install.sh <desktop-user>
#
# UNVERIFIED ON LINUX HARDWARE: the I2C port mapping (NVAPI port 1 = NVIDIA i2c adapter 1)
# has not been confirmed yet. Check `astralguard probe` reads sensible pins before relying
# on it; ASTRALGUARD_I2C_BUS=<n> in the unit selects /dev/i2c-<n> explicitly.
set -eu

USER_NAME="${1:-${SUDO_USER:-}}"
HERE="$(cd "$(dirname "$0")" && pwd)"
# Release tarball: the binary sits next to this script. Source tree: the cargo build.
BIN="$HERE/astralguard"
[ -x "$BIN" ] || BIN="$HERE/../../target/release/astralguard"

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)"; exit 1; }
[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 1; }

install -m 0755 "$BIN" /usr/local/bin/astralguard
install -m 0644 "$HERE/astralguard.service" /etc/systemd/system/astralguard.service
echo i2c-dev > /etc/modules-load.d/astralguard.conf

# Members of this group may use the dashboard (the service's socket is 0660 root:astralguard).
getent group astralguard >/dev/null || groupadd --system astralguard
if [ -n "$USER_NAME" ]; then
    usermod -aG astralguard "$USER_NAME"
    HOME_DIR="$(getent passwd "$USER_NAME" | cut -d: -f6)"
    install -d -o "$USER_NAME" "$HOME_DIR/.config/autostart"
    install -m 0644 -o "$USER_NAME" "$HERE/astralguard-ui.desktop" "$HOME_DIR/.config/autostart/astralguard-ui.desktop"
fi

systemctl daemon-reload
systemctl enable --now astralguard.service
echo "Installed. Check: systemctl status astralguard; astralguard probe (as root)."
[ -n "$USER_NAME" ] && echo "$USER_NAME must log out and back in for the astralguard group to apply."
