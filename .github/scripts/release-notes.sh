#!/bin/sh
# The GitHub release text for one version: its CHANGELOG.md section, the README screenshots as
# they were at that tag, and a link to the documentation of that version.
#
# Exits non-zero, printing why, when CHANGELOG.md has no section for the version or the
# section is empty: a release is not published without notes. Run it locally to preview:
#
#   sh .github/scripts/release-notes.sh 1.0.0
#
# usage: release-notes.sh <version> [owner/repo]
set -eu
version=${1:?usage: release-notes.sh <version> [owner/repo]}
repo=${2:-SzabolcsGergely/AstralGuard}
tag="v$version"
changelog=${CHANGELOG:-CHANGELOG.md}

# The lines after "## [<version>]" up to the next "## " heading or the link references.
section=$(awk -v v="$version" '
    $0 ~ "^## \\[" v "\\]" { found = 1; next }
    found && (/^## / || /^\[[^]]+\]: /) { exit }
    found { print }
' "$changelog")
if [ -z "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no notes for $version: add a \"## [$version] - <date>\" section." >&2
    exit 1
fi

raw="https://github.com/$repo/raw/$tag/docs/screenshots"
# Trim the blank lines around the section; keep the ones inside it.
printf '%s\n' "$section" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}'
cat <<EOF

## Screenshots

| | |
|---|---|
| ![Overview]($raw/live.png) | ![Overview while the GPU is limited]($raw/protecting.png) |
| ![Overclock]($raw/overclock.png) | ![Sag]($raw/imu.png) |
| ![Protection]($raw/protection.png) | ![Settings]($raw/settings.png) |

The images are also in \`AstralGuard-$version-screenshots.zip\` below.

## Install

Pick the file for your system below; \`SHA256SUMS.txt\` has every file's checksum. The
[documentation for this version](https://github.com/$repo/blob/$tag/README.md#install) has
the install steps, and the [changelog](https://github.com/$repo/blob/$tag/CHANGELOG.md) every
earlier release.
EOF
