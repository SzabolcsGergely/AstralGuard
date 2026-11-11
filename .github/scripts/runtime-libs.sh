#!/bin/sh
# Every library the binary can load must be installed by its package's dependencies.
#
# ldd sees only what the binary links (OpenSSL, libc). The window's libraries (Wayland, X11,
# EGL / GL, xkbcommon) are loaded at runtime by winit and glutin, so neither ldd, makepkg
# nor dpkg-shlibdeps notices when the package forgets one; the window then fails on a desktop
# that lacks it. Their sonames are string constants in the binary, so this reads them from
# there and checks each against the dynamic linker's cache. Run it in a clean container
# right after installing the package, with nothing else installed.
#
# usage: runtime-libs.sh [binary]   (default /usr/bin/astralguard)
set -eu
bin=${1:-/usr/bin/astralguard}

# Part of the NVIDIA driver, optional on purpose: without it the guard runs shutdown-only.
optional='libnvidia-ml.so.1 libnvidia-api.so.1'

# grep -a, not strings: a minimal image has no binutils.
names=$(grep -aoE 'lib[A-Za-z0-9_+-]+\.so\.[0-9]+' "$bin" | sort -u)
[ -n "$names" ] || { echo "no library names found in $bin"; exit 1; }

cache=$(ldconfig -p)
missing=0
for so in $names; do
    case " $optional " in *" $so "*) echo "skip $so (optional: NVIDIA driver)"; continue ;; esac
    if printf '%s\n' "$cache" | grep -q "^[[:space:]]*$so "; then
        echo "ok   $so"
    else
        echo "MISSING $so: loaded at runtime, but no dependency of the package installs it"
        missing=1
    fi
done
exit $missing
