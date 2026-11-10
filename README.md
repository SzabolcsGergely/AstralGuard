<p align="center">
  <img src="rust/assets/icon/astralguard-256.png" width="128" height="128" alt="AstralGuard icon">
</p>

<h1 align="center">AstralGuard</h1>

<p align="center">
  <b>Per-pin 12V-2x6 current protection for ASUS ROG Astral graphics cards.</b><br>
  Watches every 12 V pin of the power connector and steps in before a bad contact can melt it.
</p>

<p align="center">
  <a href="https://github.com/SzabolcsGergely/AstralGuard/actions/workflows/ci.yml"><img src="https://github.com/SzabolcsGergely/AstralGuard/actions/workflows/ci.yml/badge.svg?branch=master" alt="CI"></a>
  <a href="https://github.com/SzabolcsGergely/AstralGuard/releases/latest"><img src="https://img.shields.io/github/v/release/SzabolcsGergely/AstralGuard?label=release" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux-informational" alt="Windows | Linux">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license"></a>
</p>

---

The 12V-2x6 (12VHPWR) connector carries up to 600 W over six 12 V pins. If one pin makes poor
contact, the others pick up its share, and a pin running far above its rating heats the
plug until it melts. Cards usually only measure the total, so nothing notices until it
smells.

ASUS ROG Astral cards are different: a monitor chip on the board measures **each pin's
current and voltage**. AstralGuard reads that chip twice a second. When a pin runs hot, it
first **slows the GPU down** so the current drops within about a second, keeping your
session alive. It **shuts the machine down** only if that is not enough.

<p align="center">
  <img src="docs/screenshots/live.png" width="49%" alt="Dashboard: all pins within limits">
  <img src="docs/screenshots/protecting.png" width="49%" alt="Dashboard: GPU limited after a pin ran over the threshold">
</p>

## Contents

- [Features](#features)
- [Supported cards](#supported-cards)
- [Install](#install)
- [How it protects](#how-it-protects)
- [Architecture](#architecture)
- [The dashboard and tray](#the-dashboard-and-tray)
- [Overclock](#overclock)
- [Configuration](#configuration)
- [Files, logs and events](#files-logs-and-events)
- [Command line](#command-line)
- [Troubleshooting](#troubleshooting)
- [Building from source](#building-from-source)
- [Disclaimer](#disclaimer)

## Features

- **Per-pin monitoring**: all six 12 V pins, current and voltage, every 0.5 s, read
  straight from the card's own monitor chip. No HWiNFO or GPU Tweak needed.
- **Graduated response**: limit the GPU first (clock lock + minimum power limit, fully
  reversible), shut down only when limiting does not fix it. Three tiers, from a sustained
  overload to a dead short.
- **Pin imbalance warning**: one pin drifting away from the others is the early sign of a
  bad contact, often long before anything runs over its limit.
- **GPU sag detection**: the card's accelerometer notices when it tilts away from its
  installed position, since a sagging card strains the connector.
- **Flight recorder**: the last minute of per-pin samples is saved whenever the guard acts.
  That's the evidence a bug report or RMA needs.
- **Built to keep running**: protection runs in its own headless process. A watchdog
  restarts anything that crashes, and the dashboard can't take protection down with it.
- **Tray icon and dashboard**: live thermal-style view of the plug, five minutes of
  history, every setting, the log.
- **Update check**: the dashboard tells you when a new release is out. It never downloads
  or installs anything by itself.
- **Windows and Linux**: a setup wizard on Windows, `.deb` and Arch packages on Linux.

## Supported cards

Only ASUS ROG Astral cards have the per-pin monitor, so only they are supported. The card
is recognised by its PCI subsystem ID:

| Card | Subsystem ID |
|---|---|
| ROG Astral RTX 5090 OC | `0x89E31043` |
| ROG Astral RTX 5090 | `0x89FF1043` |
| ROG Astral RTX 5090 OC White | `0x8A2E1043` |
| ROG Astral RTX 5090 BTF OC | `0x8A5A1043`, `0x8A3C1043` |
| ROG Astral RTX 5090D OC | `0x89EA1043` |
| ROG Astral RTX 5090 LC | `0x89EC1043` |
| ROG Astral RTX 5090 Matrix | `0x8A611043` |
| ROG Astral RTX 5080 OC | `0x89DE1043` |
| ROG Astral RTX 5080 OC White | `0x8A2B1043` |
| ROG Astral RTX 5080 OC Hatsune Miku | `0x8A451043` |
| ROG Astral RTX 5080 | `0x89DF1043` |

Verified on hardware: ROG Astral RTX 5090 OC (readings within 0.06 A of HWiNFO). The
others share the same monitor chip and are expected to work. Reports welcome.

## Install

Get the package for your system from the
[latest release](https://github.com/SzabolcsGergely/AstralGuard/releases/latest).
`SHA256SUMS.txt` on the release page has every file's checksum.

### Windows 10 / 11

Run **`AstralGuard-<version>-setup.exe`**. It first asks who to install for:

| | Install for all users (recommended) | Install for me only |
|---|---|---|
| Program | `C:\Program Files\AstralGuard` | `%LOCALAPPDATA%\Programs\AstralGuard` |
| Start menu | everyone's | yours |
| Guard starts at sign-in for | every user who signs in, each in their own session | you |
| Administrator rights | for the whole setup | one UAC prompt, to create the logon task |

Unless you untick **Start AstralGuard when … signs in**, setup creates a logon task that
starts the guard elevated. Limiting the GPU needs administrator rights, and the task
provides them without a UAC prompt at every sign-in. Upgrades keep the choice you made
the first time.

The guard then lives in the tray. Hover the icon: `AstralGuard: Safe | Max 7.12A, Min
6.80A` means it is reading your card. Opening **AstralGuard** from the Start menu
brings up the dashboard, or starts the guard if it isn't running.

Silent install: `AstralGuard-<version>-setup.exe /VERYSILENT /ALLUSERS` (or
`/CURRENTUSER`; add `/TASKS=""` to skip the logon task).

The program is not code-signed yet, so SmartScreen may warn on first run ("More info" →
"Run anyway").

### Debian 12+, Ubuntu 22.04+

```sh
sudo apt install ./astralguard_<version>-1_amd64.deb
```

The service is enabled and started right away. The user who ran `sudo` joins the
`astralguard` group, which may use the tray and dashboard; they appear at the next login.
Add other users with `sudo usermod -aG astralguard <user>`.

### Arch Linux

```sh
sudo pacman -U astralguard-<version>-1-x86_64.pkg.tar.zst
sudo systemctl enable --now astralguard
sudo usermod -aG astralguard $USER    # then log out and back in
```

Or build it yourself with [`rust/packaging/linux/arch/PKGBUILD`](rust/packaging/linux/arch/PKGBUILD)
(`makepkg -si`).

> **Linux status: experimental.** The pins are read through the NVIDIA driver's I2C
> adapters (`/dev/i2c-*`), with SMBus block reads. Verified on a ROG Astral RTX 5090 OC
> (driver 620.70): adapter 1 carries the monitor chip. Run `sudo astralguard probe` once
> and check that it prints about 12 V and sensible currents on every pin.
>
> If the card is found but its pins don't read, the dashboard shows **Pin sensor not
> reading** with a **Scan** button (or run `sudo astralguard probe --detect`). The scan
> tries the card's own I2C adapters, keeps the one that shows 12 V on every pin, about
> 1 g on the accelerometer and a total that matches NVML's board power, and saves it
> (`i2c_adapter` and `i2c_adapter_subsystem` in `config.ini`, used only while the same card
> sits at the same PCI address). Tick **Scan automatically next time** in the scan popup
> (or Settings → System) and the guard does this by itself the next time the pins stop
> reading, for example after a driver update. `ASTRALGUARD_I2C_BUS=<n>` in a systemd
> drop-in selects an adapter explicitly and overrides the saved one.

### Updating and uninstalling

- **Update**: install the new version over the old one. Settings, the sag baseline and logs
  are kept. The dashboard checks GitHub once a day and shows an **Update available** banner
  when there is a new release; turn that off in Settings → System.
- **Uninstall on Windows**: Settings → Apps → AstralGuard → Uninstall. That removes the
  program, the logon task and the Event Viewer source. Settings and logs stay in
  `C:\ProgramData\AstralGuard`.
- **Uninstall on Linux**: `sudo apt remove astralguard` or `sudo pacman -R astralguard`.
  Settings and logs stay in `/var/lib/astralguard`.

## How it protects

Every 0.5 s the service reads all six pins and takes the highest one (`max`). Three tiers
trigger independently; the defaults:

| Tier | Trigger | Response |
|---|---|---|
| 1 · overload | `max` ≥ **9.5 A** for **15 s** | Limit the GPU, give it **10 s** to work |
| 2 · critical | `max` ≥ **13 A** for **3 s** | Limit the GPU, give it **5 s** to work |
| 3 · catastrophic | `max` ≥ **16 A** on **2 samples in a row** | Shut down at once |

For scale: at 600 W a healthy connector carries about 8.3 A per pin, ASUS warns at 9.2 A,
documented slow-cook failures ran at 11–15 A, and the most-cited melted plug measured over 22 A.

```mermaid
flowchart TD
    read["Read 6 pins (every 0.5 s)"] --> t3{"≥ 16 A on 2<br/>samples in a row?"}
    t3 -- yes --> off(["Shut down"])
    t3 -- no --> t2{"≥ 13 A for 3 s?"}
    t2 -- yes --> limit["Limit the GPU<br/>clock lock + minimum power limit"]
    t2 -- no --> t1{"≥ 9.5 A for 15 s?"}
    t1 -- yes --> limit
    t1 -- no --> read
    limit --> grace{"Still over 9.5 A when<br/>the grace window ends?"}
    grace -- yes --> off
    grace -- no --> hold["Hold at least 2 min, release<br/>after 60 s under the limit"]
    hold --> again{"Trips again within<br/>10 min of release?"}
    again -- yes --> off
    again -- no --> read
```

- **Limiting** locks the graphics clock to its floor and drops the power limit to the
  card's minimum through NVML (the API behind `nvidia-smi`). Both are put back exactly as
  they were on release. A marker file makes sure a crash while limited never leaves the
  card slowed down after a restart.
- **A fault that comes back** within 10 minutes of a release is treated as persistent, and
  the machine shuts down instead of riding it out again.
- **Shutdown** is `shutdown /s /t 5 /f` on Windows and `systemctl poweroff` on Linux. The
  next start reports that the last shutdown was an emergency stop, and why.
- **Shutdown-only mode** skips limiting entirely and keeps the same tiers and timings. It
  is also the fallback whenever GPU limiting is unavailable, for example without
  administrator rights.
- **Pin imbalance**: once a pin carries at least 6 A, a spread between the highest and
  lowest pin of 40 % or 4 A held for 5 s raises a warning. You can also set it to act like
  a tier 1 trip.
- **Sag**: a tilt of 1° from the calibrated position held for 60 s warns, 2° is serious. A
  sudden jump is reported as the PC being moved, not as sag. Sag only warns; it never
  touches the GPU.

### Status

| Tray | Status | Meaning |
|---|---|---|
| 🟢 | **Safe** | All pins within limits |
| 🟠 | **Warning** | A pin is over the threshold; the sustained timer is running |
| 🟧 | **Protecting** | The GPU is limited |
| 🔴 | **Danger** | Shutting down |
| 🔵 | **Unprotected** | Monitoring is paused (from the tray or dashboard) |
| ⚪ / 🟣 | **Unknown** | No pin readings: no supported card, or the chip stopped answering |

## Architecture

AstralGuard is a single Rust program, `astralguard`, that runs as separate processes. The
protection decision lives in a headless service that nothing else can block.

```mermaid
flowchart LR
    subgraph card["ROG Astral card"]
        chip["ITE IT8915 monitor<br/>per-pin V / A · accelerometer"]
        gpu["GPU"]
    end

    subgraph host["Your PC"]
        start["Windows: logon task (elevated)<br/>Linux: systemd unit (root)"]
        wd["<b>watchdog</b><br/>astralguard<br/>restarts crashed children"]
        svc["<b>service</b><br/>astralguard service<br/>monitor loop · response ladder<br/>imbalance · sag · flight recorder"]
        ui["<b>ui</b><br/>astralguard ui<br/>tray icon · dashboard<br/>update check"]
        data[("config.ini · logs<br/>Event Log / journal")]
    end

    gh[("GitHub<br/>releases API")]

    start --> wd
    wd -- supervises --> svc
    wd -- supervises --> ui
    svc -- "I2C reads only<br/>NVAPI / i2c-dev" --> chip
    svc -- "NVML<br/>clock lock · power limit" --> gpu
    svc --> data
    ui <-- "JSON over a local socket<br/>named pipe / unix socket" --> svc
    ui -. "once a day" .-> gh
```

| Process | Role |
|---|---|
| **watchdog** (`astralguard`) | What the logon task starts. Runs the service and the UI as children in a kill-on-close job object and restarts either one if it crashes (after 5, 10, 30, then 60 s). Touches no hardware, so it has nothing that could crash it. |
| **service** (`astralguard service`) | The protection core: reads the pins, runs the response ladder, limits the GPU, shuts down. Also runs the imbalance and sag checks, the flight recorder and the logs. Headless and makes no network connections. |
| **ui** (`astralguard ui`) | Tray icon and dashboard (egui). A client of the service, nothing more: if it hangs or crashes, protection carries on. |

The service and the UI talk over a local socket: one JSON request, one JSON response. On
Windows it is a named pipe that only elevated processes can write to, so an ordinary
program cannot pause the guard. On Linux it is `/run/astralguard/astralguard.sock`, open
to root and the `astralguard` group. On Linux, systemd does the watchdog's job
(`Restart=on-failure`) and the UI starts from the desktop session's autostart. There
the tray is its own process and the dashboard window another one (`astralguard ui
--window`), started from the tray and gone when it is closed: Wayland gives an app no way
to hide its window. The tray is a StatusNotifierItem: left click opens the dashboard,
right click the menu.

**Read-only hardware access.** The sensor code only ever sets a register pointer and reads
from the monitor chip at I2C address `0x2B`; it never writes a data register. NVAPI's I2C
write function is deliberately not bound at all. The GPU is limited through NVML only, the
documented driver API.

## The dashboard and tray

<p align="center">
  <img src="docs/screenshots/imu.png" width="49%" alt="Anti-sag tab: bubble level, tilt per axis, sag">
  <img src="docs/screenshots/settings.png" width="49%" alt="Settings tab">
</p>

- **Overview**: a thermal-camera style view of the plug (colour = current, scaled to the
  thresholds), max / min / delta / total power, average voltage, response state,
  imbalance and sag, and five minutes of history.
- **Overclock**: an optional undervolt, power limit and memory offset ([below](#overclock)).
- **Anti-sag**: bubble level, the card's angle on each axis, sag against the calibrated
  position, and a minute of history. **Calibrate** sets the reference.
- **Settings**: every protection value (checked, and confirmed when it weakens
  protection), response mode, autostart, theme, window behaviour, update check.
- **Log**: the guard's log and the flight recorder dumps.

The **tray icon** is a dot in the status colour. Its menu: **Open Dashboard**, **Open
Log**, **Pause / Resume** monitoring, **Exit**. Pausing is visible everywhere and reminds
you every 30 minutes.

## Overclock

The **Overclock** tab is optional and separate from protection. It can set three things, each
with its own tick box: an **undervolt** (the clock the card runs at a range of voltages), a
**power limit** in watts, and a **memory offset**. A setting that is not ticked is never
touched.

- **Undervolt**: raises the V/F curve between two voltages so the card reaches the target
  clock at a lower voltage. It uses NVIDIA's undocumented NVAPI curve calls. Before the first
  curve write on each driver version, AstralGuard writes 15 MHz to one point in
  the middle of the curve, reads the whole curve back (the offsets and, separately, the clocks the driver
  reports), checks that only that point moved, and restores it. If that check fails, only the
  undervolt is switched off, and it stays off on that driver version. Any
  per-point offset beyond ±1000 MHz is refused, not clamped. It is also refused while a core
  clock offset is set outside AstralGuard. The probe writes 15 MHz because the driver does not
  apply a 5 MHz offset.
- **Power limit**: the board power limit in watts, through NVML.
- **Memory offset**: in Afterburner units. AstralGuard sends twice the number to the driver,
  so `2000` here is what the driver calls `4000`.

AstralGuard records what it changed, the value it applied and the value from before in
`overclock.active`, next to `mitigation.active`. Unticking a setting puts back the value from
before, only for a setting AstralGuard applied itself, and only while the card still runs the
value AstralGuard applied: after a reboot, or a change by another tool, the card's current
value is left as is. A marker written for another card is ignored.

**Apply now** saves and writes in one step, and it changes only the overclock settings. While
the guard is limiting the GPU it is refused and nothing is saved; if the guard starts limiting
between the save and the write, the settings stay saved and are applied when the guard releases
the GPU. It runs on the service's monitor thread, so it never overlaps the guard's own limit.
At start the saved settings are applied once, after `oc_apply_delay_seconds`; if the GPU is
limited by then, or cannot be opened, the service waits and retries every 30 s. After every
release from limiting, the saved settings are applied again (values already in effect are not
rewritten). The same goes for a resume from sleep, after the same delay: the driver puts the
power limit back to stock when the machine wakes up. With nothing ticked and nothing managed, AstralGuard makes no NVML or NVAPI calls
for the overclock at all, and the tab reads the GPU only while it is open.

Because these calls run on the monitor thread, the pin reads wait for them. Every overclock
call there that takes longer than 250 ms is logged as a warning with its duration, and the
dashboard shows **Guard loop not responding** when the monitor loop has not moved for more
than 5 s.

Quitting AstralGuard leaves the overclock in place; the driver drops it at reboot. If the
guard trips while an overclock is active, the log and the flight dump say so.

The NVAPI curve layouts were verified on Linux (2026-11-09, ROG Astral RTX 5090 OC, driver
620.70.06) and on Windows (2026-11-10, same card, driver 621.14). Only the core voltage readout
is unverified on Windows.

## Configuration

Settings live in `config.ini`. Edit them from the dashboard: it validates every value,
applies it at once, and keeps any keys and comments it doesn't know. To edit the file by
hand, stop the guard first.

| Key | Default | Meaning |
|---|---|---|
| `threshold_amps` | `9.5` | Tier 1 threshold per pin |
| `sustained_seconds_required` | `15` | How long tier 1 must hold |
| `response_mode` | `tiered` | `tiered` (limit, then shut down) or `shutdown_only` |
| `critical_amps` / `critical_seconds` | `13` / `3` | Tier 2 |
| `catastrophic_amps` | `16` | Tier 3 (two samples in a row) |
| `mitigation_grace_seconds` | `10` | Tier 1: time for the limit to work |
| `mitigation_clear_seconds` | `60` | Time under the threshold before releasing |
| `mitigation_min_hold_seconds` | `120` | Minimum time limited |
| `retrigger_window_seconds` | `600` | A new trip this soon after a release shuts down |
| `imbalance_enabled` | `true` | Pin imbalance check |
| `imbalance_min_amps` | `6` | Only judged once a pin carries this much |
| `imbalance_percent` / `imbalance_delta_amps` | `40` / `4` | Trip on either |
| `imbalance_sustained_seconds` | `5` | How long the spread must hold |
| `imbalance_response` | `warn` | `warn`, or `act` (treat as a tier 1 trip) |
| `sag_enabled` | `true` | Sag detection |
| `sag_warn_degrees` / `sag_serious_degrees` | `1` / `2` | Tilt from the calibrated position |
| `sag_sustained_seconds` | `60` | How long the tilt must hold |
| `sag_card_length_mm` | `0` | For the mm figure; `0` = the detected card's spec |
| `sensor_auto_scan` | `false` | Linux: scan for the pin monitor by itself when the card is found but its pins stop reading, once each time (not at every start) |
| `oc_uv_enabled` | `false` | Overclock: apply the undervolt |
| `oc_uv_from_mv` / `oc_uv_to_mv` | `820` / `895` | Voltage range the undervolt covers (mV) |
| `oc_uv_target_mhz` | `2827` | Clock the card should reach across that range |
| `oc_power_enabled` | `false` | Overclock: apply the power limit |
| `oc_power_w` | `520` | The power limit (W) |
| `oc_mem_enabled` | `false` | Overclock: apply the memory offset |
| `oc_mem_offset_mhz` | `2000` | Memory offset in Afterburner units (the driver gets twice this) |
| `oc_apply_delay_seconds` | `30` | Wait after the service starts, or the machine wakes up, before applying the overclock |

The sag baseline (`sag_baseline`, `sag_baseline_subsystem`) is written by **Calibrate** and
re-taken automatically when a different card is installed. `oc_vf_verified_driver` is written
by the service after the undervolt's compatibility check passes, so the check runs again only
after a driver update. `oc_vf_failed_driver` is written when the check finds a write landing
in the wrong place: on that driver version the check does not run again and the undervolt stays
off until a driver update (or until the key is removed).

Window preferences (theme, size, which tab, pin order, close action, update check) are kept per user in
`%APPDATA%\AstralGuard\ui.json` / `~/.config/astralguard/ui.json`.

## Files, logs and events

| | Windows (installed) | Linux |
|---|---|---|
| Settings | `C:\ProgramData\AstralGuard\config.ini` | `/var/lib/astralguard/config.ini` |
| Log (rotates at 2 MB, keeps 5) | `…\logs\astralguard.log` | `/var/lib/astralguard/logs/astralguard.log` |
| Watchdog / crash traces | `…\logs\watchdog.log`, `crash_trace.log` | `logs/crash_trace.log`; restarts in the journal (`journalctl -u astralguard`) |
| Flight recorder | `…\logs\flight_<time>_<event>.csv` | same, under `logs/` |
| Daily sag median | `…\logs\sag_history.csv` | same, under `logs/` |
| GPU limit / overclock markers | `mitigation.active`, `overclock.active` (data folder) | same, `/var/lib/astralguard` |
| Last emergency shutdown | `last_shutdown.json` (data folder) | same |
| Installer steps | `…\logs\setup.log` | none |

A copy of `astralguard.exe` with a `config.ini` next to it keeps everything in its own
folder (portable use). `ASTRALGUARD_HOME` overrides the location.

On Windows, important events also go to the **Event Viewer** (Windows Logs → Application,
source *AstralGuard*):

| ID | Event |
|---|---|
| 1000 / 1003 | Started / stopped |
| 1002 / 1004 | Card connected / sensor lost |
| 1005 | Restarted after a crash |
| 3001 / 3002 | Pin over the threshold / still over |
| 4001 | Load cleared, timer reset |
| 5090 | Emergency shutdown |
| 5091 | The last shutdown was an emergency stop (reported at the next start) |
| 6001 | Critical current (tier 2) |
| 6002 | GPU limited (tier 1 or 2) |
| 6003 | The limit brought the current down; holding |
| 6004 | Tiered mode selected, but the GPU cannot be limited: running shutdown-only |
| 6005 | GPU limit released |
| 6006 | Restored a GPU limit left by a crash (`mitigation.active`) |
| 7001 / 7002 | GPU sag warning / serious |
| 7003 | Card orientation changed suddenly |
| 7004 | Sag creeping up over weeks |
| 7005 | Sag baseline calibrated |
| 7006 | Sag back to normal |
| 7007 | Card off level at calibration |
| 8001 / 8002 | Pin imbalance / cleared |
| 9001 | Error: the monitor loop or an overclock step panicked (the guard carries on) |

On Linux the same events go to the journal as `[id] message`.

## Command line

| Command | |
|---|---|
| `astralguard` | Watchdog + service + tray. Started again while running, it opens the dashboard. |
| `astralguard watchdog [--no-ui]` | The watchdog explicitly; `--no-ui` supervises the service only |
| `astralguard service` | The service alone (what systemd runs on Linux) |
| `astralguard ui [--show]` | Tray and dashboard alone |
| `astralguard ui --demo` | Dashboard with a simulated card: no hardware, no service |
| `astralguard probe` | Read the card a few times and print pins, volts and tilt. Read-only, safe next to a running guard. |
| `astralguard probe --detect [--dry-run]` | Linux: find the I2C adapter that carries the pin monitor and save it (what the dashboard's **Scan** does) |
| `astralguard status [--exit]` | Ask the running service for its state, or stop it (elevated / root) |
| `astralguard register [--all-users]` / `unregister` | Windows, elevated: create / remove the logon task (for this user, or for everyone who signs in) and the Event Log source. The installer uses these. |

## Troubleshooting

- **Tray says *Unknown (waiting for pin sensors)***: the card could not be read. Check that
  it is a supported Astral card and the NVIDIA driver is loaded; the log says why.
  `astralguard probe` shows exactly what the chip answers.
- **Banner says *Pin sensor not reading***: the card is there but its pin monitor gives
  no valid data (Linux). Press **Scan**; the result lists what each I2C adapter answered.
- **Banner says *GPU limiting unavailable* (Response: *Shutdown only*)**: the guard is not running
  elevated (Windows) or as root (Linux), so it can only shut down. Start it through the
  logon task (Start menu) or the systemd unit.
- **The dashboard says *Guard service not running***: on Windows, start AstralGuard from
  the Start menu. On Linux your user is probably not in the `astralguard` group yet, or
  hasn't logged in again since joining.
- **Pin numbers look mirrored**: which physical socket is "Pin1" has not been verified.
  Settings → Window → **Mirror connector pin order** flips the drawing. Protection doesn't
  depend on it.

## Building from source

```sh
git clone https://github.com/SzabolcsGergely/AstralGuard
cd AstralGuard/rust
cargo build --release
```

[`CLAUDE.md`](CLAUDE.md) is the contributor guide: toolchains for Windows and Linux,
the Linux build dependencies, tests and lints, the code layout, the safety rules the code
follows, packaging and how releases are cut.

## Disclaimer

AstralGuard is an independent project. It is **not affiliated with, endorsed by or
supported by ASUS or NVIDIA**; ROG and Astral are trademarks of ASUSTeK Computer Inc.

It reduces risk; it does not remove it. It cannot see a problem the card's sensors cannot
measure: temperature at the plug, a damaged cable, or the PSU side of the connector. It
needs to be running to help. A connector that ever triggers it should be inspected and
reseated or replaced. The software is provided as is, under the [MIT license](LICENSE),
without warranty of any kind; you use it at your own risk.

Large parts of this project were written with AI assistance (Claude) and reviewed and
tested on real hardware by the author.
