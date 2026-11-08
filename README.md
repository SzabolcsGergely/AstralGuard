# AstralGuard

[![CI](https://github.com/SzabolcsGergely/AstralGuard/actions/workflows/ci.yml/badge.svg)](https://github.com/SzabolcsGergely/AstralGuard/actions/workflows/ci.yml)

*Named for the only cards it can protect: ASUS ROG Astral cards are the ones with per-pin
current sensing. Not affiliated with or endorsed by ASUS.*
**Standalone GPU 12VHPWR Pin Safety Monitor for Windows (Linux experimental)**

A lightweight Windows watchdog that continuously monitors **per-pin 12VHPWR current**, read **directly from the card's own sensor chip** (no HWiNFO or other monitoring software needed). When a pin goes over threshold it **slows the GPU down within about a second**, and escalates to a **forced, safe system shutdown** if that does not fix it.

Built to protect modern high-power GPUs from connector overheating and burnout caused by sustained current imbalance.

**IMPORTANT: Only ASUS cards that carry ASUS's per-pin current sensing and accelerometer are supported: the ROG Astral RTX 5080 / 5090 family and its derivatives (OC, White, BTF, LC, 5090D, ROG Matrix). On any other card the guard cannot read anything and provides no protection.**

---

## Why this exists

Sustained current above ~**9.5 A per 12VHPWR pin** has been shown to cause:
- connector overheating
- melting
- permanent GPU damage

GPU firmware, drivers, and PSU protections **do not reliably mitigate this scenario**.

This tool adds a last-line, OS-level safety net by:
- reading **per-pin current telemetry**
- detecting **sustained**, not transient, overloads
- **cutting the card's power draw immediately**, which is far quicker than a shutdown and
  does not cost you your session
- **powering down the system** if the current stays dangerous anyway

---

## Key Features

- Monitors **GPU 12VHPWR Pin1-Pin6 Current**
- **Standalone: reads the card's own sensor chip directly, no HWiNFO required**
- Requires **sustained threshold breach** (default: 15 s)
- **Slows the GPU down first** (clock lock plus minimum power limit) and only shuts down if
  that does not bring the current back under control
- **Severity tiers**: the higher the current, the faster the response (15 s / 3 s / immediate)
- **Fully reversible**: your exact power limit is restored and the clocks unlocked once the
  fault clears, including after a crash
- **Tells you why**: after an emergency shutdown, the next start shows what happened and
  when, so a trip while you were away is not a mystery
- **Flight recorder**: the last minute of per-pin samples is saved to `logs/` whenever the
  guard acts, evidence for a bug report or an RMA claim
- **Pin imbalance warning**: flags an uneven split between the highest and lowest pin (the
  early sign of a bad contact), modelled on the WireView Pro II. Warns by default, or
  optionally responds like overcurrent, see [Pin imbalance warning](#pin-imbalance-warning)
- **GPU sag detection**: the card's own accelerometer warns when
  the card tilts away from its installed position, since a sagging card strains the connector.
  Warn-only, see [GPU sag detection](#gpu-sag-detection)
- **Force-closes blocking apps** during shutdown so nothing can stall it
- **Silent background operation**
- **Tray icon** with live status, and a **dashboard** for everything else
- **Every setting** changeable from the dashboard, validated, applied without a restart
- **Autostart toggle** without reinstalling
- **Persistent configuration** saved to `config.ini`
- Pause / Resume from tray
- Toast notifications
- **Windows Event Viewer logging**
- Quiet rotating file logs
- **Detects sensor loss** (I2C failure, driver reset) and auto-reconnects; never reports OK on stale data
- **Resets its timer across sleep/resume** instead of trusting a stale reading
- **Single-instance guard** (a second copy exits instead of double-monitoring)
- **Watchdog**: restarts the guard if it ever crashes, and says so
- **One native program** (Rust), no runtime to install

---

## How it is built

AstralGuard is one Rust program, `astralguard.exe` (source in [`rust/`](rust/)). Version 2.0
replaced the earlier Python implementation; the protection logic is a line-by-line port and
reads the same `config.ini`, logs, sag baseline and history.

It runs as separate processes, so a dashboard problem can never reach the protection loop:

| Command | What it is |
|---|---|
| `astralguard` (or `astralguard watchdog`) | What the logon task starts. Runs the two below and restarts either one if it crashes. `--no-ui` runs the service only. |
| `astralguard service` | The protection core: pins, response ladder, GPU limiting, shutdown, sag, imbalance, logs, Event Log. Headless. |
| `astralguard ui` | Tray icon and dashboard. Talks to the service over a local pipe that only elevated processes can write to. |
| `astralguard probe` | Reads the card a few times and prints pins, volts and the accelerometer. Read-only. |
| `astralguard status` | Asks the running service for its state (run elevated). `--exit` stops it. |

**Linux (experimental):** install the `.deb` or Arch package (see
[Installation](#installation)). The service runs as a root systemd unit (restart-on-failure
does the watchdog's job), the tray and dashboard start in the desktop session and talk to the
service through a socket in `/run/astralguard` open to the `astralguard` group. Settings and
logs live in `/var/lib/astralguard`. Pins are read through the NVIDIA driver's I2C adapters
(`/dev/i2c-*`), the GPU is limited through `libnvidia-ml`. **Not yet verified on Linux
hardware:** the sensor bus is assumed to be "NVIDIA i2c adapter 1"; check
`sudo astralguard probe` first. `ASTRALGUARD_I2C_BUS=<n>` (in a systemd drop-in for the unit)
selects one explicitly.

## Requirements

### Hardware Compatibility
**This utility ONLY works with ASUS ROG Astral cards.** These carry an ITE IT8915FN monitoring
chip that measures each 12VHPWR pin individually. Recognized models (by PCI subsystem ID):

| Subsystem ID | Model |
|---|---|
| `1043:89E3` | ROG Astral RTX 5090 OC |
| `1043:89EA` | ROG Astral RTX 5090D OC |
| `1043:8A61` | ROG Astral RTX 5090 Matrix |
| `1043:89EC` | ROG Astral RTX 5090 LC |
| `1043:89DE` | ROG Astral RTX 5080 OC |
| `1043:8A2E` | ROG Astral RTX 5090 OC White |
| `1043:8A2B` | ROG Astral RTX 5080 OC White |
| `1043:8A45` | ROG Astral RTX 5080 OC Hatsune Miku |
| `1043:8A5A` | ROG Astral RTX 5090 BTF OC (see BTF note) |
| `1043:8A3C` | ROG Astral RTX 5090 BTF OC, second id revision of the same card (see BTF note) |
| `1043:89DF` | ROG Astral RTX 5080 (non-OC) |

**BTF note:** the BTF card is supported when it draws power through the normal 16-pin cable
in a standard motherboard, which is where its card-side sensors sit. Installed in an actual
BTF motherboard (power through the GC-HPWR slot connector), the load bypasses those sensors:
the guard would read near zero on every pin and could not protect anything. Do not rely on
this tool for a GC-HPWR powered card.

Have an Astral variant that is not in this table (the guard sits purple)? Run
`nvidia-smi --query-gpu=name,pci.sub_device_id --format=csv` and open an issue with the
output; adding a verified id is a one line change.

No other GPU exposes per-pin current, including non-Astral RTX 5080/5090 cards. On unsupported
hardware the guard cannot protect anything and will say so rather than pretend to work.

### OS
- Windows 10 / 11

### Software
- NVIDIA graphics driver (the guard uses NVAPI, which is already part of every NVIDIA driver
  install; nothing extra is needed)
- HWiNFO64 is **not** needed. If it happens to be running, the guard uses it only as a second
  opinion; see [How it reads the sensors](#how-it-reads-the-sensors).

---

## How it reads the sensors

The guard reads the card's own IT8915FN monitor chip over the GPU's I2C bus using NVIDIA's
driver API. The same chip relays the accelerometer used for sag detection, so one read-only
path covers everything. It never writes to the I2C bus.

That chip is why only ASUS ROG Astral cards and their derivatives are supported: other cards
either have no per-pin sensing or do not expose it. The card model is named in the log at
startup and shown in the dashboard header.

**Crosscheck against HWiNFO (optional).** If HWiNFO is running with Shared Memory enabled, the
guard compares its own readings against HWiNFO's once a minute and logs an hourly summary, or
a warning if they ever disagree by 2 A or more. This is observational only: it never feeds a
decision, and the guard does not care whether HWiNFO is running.

---

## How it responds

Shutting a machine down takes a while, and it costs you whatever you were doing. Telling the
GPU to stop pulling current takes about a second and costs you nothing but frame rate. So the
first response to an over-current pin is a **reversible GPU slowdown**: the graphics clock is
locked to the card's floor and the power limit is dropped to the card's minimum, both through
NVIDIA's own management API. Shutdown remains the escalation for when that is not enough.

### Severity tiers

The tiers exist because some currents are too high to sit through a 15 second window.

| Tier | Trigger (any single pin) | Response |
|---|---|---|
| 1 - elevated | >= 9.5 A sustained 15 s | Limit the GPU. Shut down if the pin is still at or above 9.5 A after 10 s |
| 2 - critical | >= 13 A sustained 3 s | Limit the GPU right away. Shut down if the pin is still at or above 9.5 A after 5 s |
| 3 - catastrophic | >= 16 A on 2 consecutive samples (about 1 s) | Limit the GPU and shut down immediately |

For scale: spec load at 600 W is about 8.3 A per pin, ASUS's own warning fires at 9.2 A,
documented slow-cook failures run 11-15 A, and the melted connector everyone has seen measured
over 22 A on one wire. Tier 3 needs two consecutive samples so a single bad read can never
reach it, and readings above 30 A are discarded as garbage before any of this runs.

### Getting the GPU back

Once limited, the card stays limited for at least **120 s**, and is only released after all
pins have been below the threshold for **60 s**. Currents drop the instant the clocks do, so
releasing on current alone would just flap. On release your power limit is restored to
**exactly the value it had**, not to the card's default, so a custom limit survives.

One caveat for undervolters: the emergency slowdown replaces any clock lock you set yourself
(`nvidia-smi -lgc` style), and the release returns the card to its automatic clock management
rather than to your lock. NVIDIA's API offers no way to read an existing lock back, so the
guard cannot restore what it cannot see. Curve based undervolts (MSI Afterburner and similar)
are not affected. Every mitigation announces itself with a toast and a log entry, so you will
know to reapply your lock afterwards.

If a tier triggers again within **600 s** of a release, that is treated as a persistent fault
rather than a transient: the guard limits the card and shuts down. Pausing the guard, or a
suspend/resume, releases the limit instead of holding it, since in both cases the decision has
gone back to you.

If the guard is killed while the GPU is limited, it leaves a `mitigation.active` marker file
behind and restores everything the next time it starts. A reboot clears both changes on its
own, since neither survives a driver reload.

**Why it drops so deep:** the power limit alone cannot protect you. All six pins share one
12 V rail, and how the current splits between them is set by contact resistance, which is
exactly the thing that has gone wrong. At the card's 400 W minimum limit, a badly seated pin
can still carry 11-13 A while the total draw looks fine. Only near idle draw is safe under
any imbalance, and that is what the clock floor delivers. Validated mid game (Alan Wake 2,
roughly 500 W, about 7 A on the busiest pin): on engage the clock hit the floor in 1 second,
the worst pin fell under 2 A inside 4 seconds and settled at 0.86 A, and on release the game
went straight back to full clocks with the power limit restored to exactly its prior value.

**The drop targets are deliberately not configurable.** The power limit target is whatever
minimum the card reports and the clock target is its lowest supported frequency, both read
from the card at startup. There is no useful way to tune an emergency response except weaker,
and the session survives floor clocks for two minutes either way. What you *can* configure is
when it happens, never how hard.

### After it fires

An emergency shutdown leaves a note. At the next start the guard announces exactly what
happened and when, as a notification, a log warning and an Event Viewer entry, then removes
the note. Without this, a shutdown that fires while you are away looks like a Windows crash,
and the bad cable gets plugged straight back into a heavy load. If you see that message,
check the connector seating before the next long session.

Whenever the guard limits the GPU or shuts down, it also writes the last minute of per-pin
samples to a `flight_*.csv` in the `logs` folder (the ten most recent are kept, and nothing
is written to disk in normal operation). That file shows what every pin was doing in the
sixty seconds before the guard acted, which is exactly what a bug report or an RMA claim
needs.

### Response mode

In the dashboard's **Settings** tab, under **Response**, pick one:

- **Limit GPU, then shut down** *(default, `tiered`)*
- **Shut down only** (`shutdown_only`)

The change takes effect on the next poll, no restart. The severity tiers apply in **both**
modes, so shutdown-only is still meaningfully faster than v1.0 at high currents: 13 A gets you
a shutdown in 3 s rather than 15. Shutdown-only never touches the NVIDIA management API at
all, which makes it the clean rollback if anything about the GPU limiting misbehaves on your
system.

### Elevation

Changing clocks and power limits requires an elevated process. The installer's scheduled task
already runs at the highest privileges, so a normally installed guard can do it. A guard
started by hand from a normal console cannot: it logs a warning, the tray tooltip reads
`shutdown-only`, the dashboard shows why, and protection falls back to shutting down. It never
silently pretends to have a capability it does not have.

---

## Installation

Download the package for your system from the
[releases page](https://github.com/SzabolcsGergely/AstralGuard/releases):

| System | File |
|---|---|
| Windows 10/11 (x64) | `AstralGuard-<version>-setup.exe` |
| Debian, Ubuntu and derivatives | `astralguard_<version>-1_amd64.deb` |
| Arch Linux and derivatives | `astralguard-<version>-1-x86_64.pkg.tar.zst` |

`SHA256SUMS.txt` on the same page lists every file's checksum.

### Windows

Run **`AstralGuard-<version>-setup.exe`** and follow the wizard (it asks for administrator
rights). It installs `astralguard.exe` to `C:\Program Files\AstralGuard`, adds a Start menu
entry and, unless you untick **Start AstralGuard when I sign in**, creates the
**"AstralGuard"** scheduled task that:

- starts the guard at sign-in, elevated (needed to limit the GPU; see [Elevation](#elevation)),
  with no console window
- restarts it if it fails to start (up to 999 times, 1-minute interval); crashes after that
  are the [watchdog's](#crash-recovery-the-watchdog) job
- runs indefinitely: the default 72-hour Task Scheduler execution limit is disabled, and
  battery-power settings will not stop it

The last page offers to start it right away. Settings and logs live in
`C:\ProgramData\AstralGuard` (Start menu → **AstralGuard settings and logs**). Opening
**AstralGuard** from the Start menu while it runs brings up the dashboard.

Silent install, e.g. for scripted setups: `AstralGuard-<version>-setup.exe /SILENT`
(`/VERYSILENT` shows nothing at all; `/TASKS=""` skips the logon task).

**After installation:** the guard runs from the system tray. On a supported card nothing else
is needed; hover the icon and the tooltip should read `AstralGuard: Safe | Max ...`. If it
says `Unknown (waiting for pin sensors)`, the card could not be read; the log says why.

**Coming from 12VHPWR Guard or PinGuard** (the app's earlier names): the setup stops and
replaces their scheduled tasks. A copy that kept `config.ini` next to the program keeps using
it from there when run from that folder; for the installed copy, move `config.ini` to
`C:\ProgramData\AstralGuard` to keep your thresholds.

### Linux

**Debian / Ubuntu:**

```
sudo apt install ./astralguard_<version>-1_amd64.deb
```

The service is enabled and started right away. Whoever installed it with `sudo` is added to
the `astralguard` group, which may use the tray icon and dashboard; they appear at the next
login. Add other users with `sudo usermod -aG astralguard <user>`.

**Arch:**

```
sudo pacman -U astralguard-<version>-1-x86_64.pkg.tar.zst
sudo systemctl enable --now astralguard
sudo usermod -aG astralguard $USER
```

Following Arch practice, the package does not start the service by itself. To build it from
source instead, use [`rust/packaging/linux/arch/PKGBUILD`](rust/packaging/linux/arch/PKGBUILD)
(`makepkg -si`).

Both packages install the binary to `/usr/bin/astralguard`, the `astralguard.service` unit,
the `i2c-dev` module load, the `astralguard` group (systemd-sysusers) and the tray's
autostart entry (`/etc/xdg/autostart`).

### Upgrading

Install the new version over the old one: run the new setup on Windows (it stops the running
guard first, since the program file is locked while it runs), or install the new package on
Linux (the service is restarted). Settings, the sag baseline and logs are kept.

**Update check.** The dashboard asks GitHub's releases API once shortly after it starts
and then once a day whether a newer release exists. If one does, you get a notification
and an **Update available** banner with **Download** (opens the release page) and **Skip**
(no banner for that version again). It never downloads or installs anything itself. Turn it
off in Settings → System → **Check for updates daily**; **Check now** checks on demand. Only
the dashboard process does this: the protection service makes no network connections at
all, so a failed check can never affect monitoring.

The version you are running is shown at the top of the tray menu and in the first log line
at startup.

### Uninstalling

- **Windows:** Settings → Apps → **AstralGuard** → Uninstall (or "Add or remove programs").
  It stops the guard and removes the program, the scheduled task and the Event Viewer source.
  `C:\ProgramData\AstralGuard` (settings and logs) stays; delete it for a clean slate.
- **Debian / Ubuntu:** `sudo apt remove astralguard` (`purge` also drops the package's config).
- **Arch:** `sudo pacman -R astralguard`.

On Linux, `/var/lib/astralguard` (settings and logs) stays until you delete it.

### Portable use (Windows)

`astralguard.exe` also runs without installing: copy it out of `C:\Program Files\AstralGuard`
or build it (see [Building from source](#building-from-source)). Run it elevated (right-click,
Run as administrator); without elevation it monitors and can shut down, but cannot limit the
GPU. A copy with a `config.ini` next to it keeps its settings and `logs\` in that folder;
otherwise it uses `C:\ProgramData\AstralGuard` (`ASTRALGUARD_HOME` overrides either).
`astralguard register` (elevated) creates the logon task for that copy, and
`astralguard unregister` removes it again.

### Crash recovery (the watchdog)

`astralguard.exe` without arguments is the watchdog: it runs `astralguard service` and
`astralguard ui` and starts either one again if it dies. A clean service exit (Exit from the tray
or dashboard, or the guard shutting the machine down) ends everything; anything else counts as
a crash, and it is restarted after 5 s (10, 30, then 60 s if it keeps failing; a run of 10
minutes resets that). The restarted service logs why, writes an Event Log warning (ID 1005)
and shows a "Restarted after a crash" notification. A dashboard that crashes is restarted the
same way and never affects the service. Starting `astralguard.exe` again while the guard runs
just opens the dashboard. The watchdog's own log is `logs\watchdog.log`; a panic leaves a
backtrace in `logs\crash_trace.log`. Both children run in a job object, so stopping the
scheduled task stops them too.

---

## Tray Icon Status

Every colour has a one-word label for how safe the connector is, shown in the tooltip, the
dashboard header and the thermal view's corner tag, with the reason next to it:

| Color | Label | Meaning |
|------|-------|--------|
| Green | **Safe** | All pins within limits |
| Amber | **Warning** | Over threshold, the sustained timer is running |
| Orange | **Protecting** | The guard has limited the GPU and is waiting for the current to clear |
| Red | **Danger** | Shutdown triggered |
| Blue | **Unprotected** | Monitoring paused |
| Purple | **Unknown** | The card's sensors cannot be read / waiting to reconnect (not monitoring) |
| Gray | **Unknown** | Card connected, but it returned no pin readings |

**Purple vs. Gray:** purple means the guard **cannot read the card**: it is not a supported
ASUS card, the driver is being reinstalled, or the chip stopped answering. The guard keeps
retrying every few seconds. Gray means the card connected but returned no pin readings. In
both cases safety cannot be judged and **no protection is active**.

Hovering the icon shows the current status and the highest and lowest pin current, e.g.
`AstralGuard: Safe | Max 8.12A, Min 7.90A`. Otherwise the reason follows the label and the pins
are named, e.g. `AstralGuard: Warning (over threshold) | Max Pin3 9.10A, Min Pin6 8.40A` or
`AstralGuard: Protecting (GPU limited) | ...`. An active imbalance warning and the sag reading
are appended, and if a tier crossing would go straight to shutdown (either because you chose
that, or because the guard is not elevated) the tooltip says `shutdown-only`.

## Dashboard

A window with everything in one place, opened from the tray (**Open Dashboard**, or click
the icon). Flat and dark by default: small uppercase captions, big values, one accent colour.

- **Live** - the 12V-2x6 connector as a thermal-camera view, face on: each 12 V socket glows
  with its current on a thermal colour scale anchored to your settings (normal load
  magenta-red, the threshold orange, tier 2 yellow, tier 3 white-hot), the heat spreading into
  the housing and the ground socket beside it. The scale on the right shows the anchors.
  Crosshairs mark the highest (MAX) and lowest (MIN) pin. Under
  each socket: its current and a bar with threshold and tier 2 marks. The colours show
  current, not temperature: the card has no per-pin temperature sensors.

  A pin far below the others (the imbalance check's own rule: 40% / 4 A, once a pin carries
  6 A) is a cold, dark hole outlined in red and labelled **NO CURRENT** (under 1 A) or
  **LOW**, because a burnt-open wire reads about 0 A. A tag in the corner shows SAFE,
  IMBALANCE, WARNING, PROTECTING · TIER n, DANGER, UNPROTECTED or UNKNOWN. The view stays dark in the
  light theme, like a camera viewfinder.

  Below it: max / min / max delta and total power, the highest and lowest pin over the last
  5 minutes, and the imbalance, sag and response state. A banner appears while paused,
  while the GPU is limited, and on an imbalance or sag warning.

  The pins are drawn in the order the card reports them (the same Pin1-Pin6 as HWiNFO and
  GPU Tweak III), and the layout (latch over the ground row, sense pins on the far side)
  follows the 12V-2x6 pinout drawing. Which physical socket each number is has not been
  verified, and GPU Tweak III's own connector graphic is unnumbered; **Connector view: mirror
  the pin order** in Settings flips it (`Mirror connector pin order`).
- **IMU** - the accelerometer: X, Y, Z as each axis's angle to horizontal (0° = level,
  ±90° = straight down or up), absolute tilt, sag since calibration, a bubble level with the sag warning and serious angles drawn as rings around
  the calibrated position, and a 60 s strip chart per axis. **Fast polling** reads the sensor
  about 10 times a second for 60 s, then switches itself off. The fast reads happen in the
  monitor loop's idle time between pin ticks (pins are still read every 0.5 s), are skipped
  while the GPU is limited, never count as sensor failures, and are for display only: sag
  detection keeps its own 5 s samples.
- **Settings** - the overcurrent threshold and sustained time, the tier 2 current and time
  and the tier 3 current, response mode, every imbalance and sag value, sag calibration and
  autostart. **Apply** sends only what changed, is validated by the service, and asks before
  anything that reduces protection: raising the threshold, the sustained time or any tier
  value above its default. Every change is logged. The thresholds must rise (threshold < tier 2 <
  tier 3). Tier changes take effect on the next sample.
- **Log** - the recent log with warnings highlighted, and the flight recorder dumps.

**Window preferences** (Settings tab, apply at once). They belong to the window, not the
guard, so they are saved per user in `%APPDATA%\AstralGuard\ui.json`
(`~/.config/astralguard/ui.json` on Linux), not in `config.ini`:

| Preference | Options | Default |
|---|---|---|
| Theme | System (follows the Windows app theme), Dark, Graphite, Midnight, Violet, Light | System |
| Close button | Hide to tray and keep protecting / Exit the guard (asks first) | Hide to tray |
| Minimize to tray | Minimizing hides the window; the tray icon brings it back | off |
| Open on start | Show the window when the guard starts | off |
| Always on top | | off |
| Mirror connector pin order | Draw the connector's pins in reverse order | off |

Window size, position and the last tab are remembered.

Hiding or closing the window never affects monitoring: the dashboard is a separate process
that reads the state the service publishes and changes settings through it. It never talks
to the hardware.

---

## Tray Menu

Right-click the tray icon:

- **Open Dashboard** - Opens the [dashboard window](#dashboard). Clicking the tray icon
  does the same
- **Open Log** - Opens the log file
- **Pause / Resume** - Temporarily pause monitoring (asks first; releases the GPU if it is
  limited). While paused, a reminder fires every 30 minutes; a paused guard protects nothing
- **Exit** - Stop the guard (asks first)

All settings are in the dashboard's **Settings** tab.

### Start with Windows

The **Start with Windows** switch in the Settings tab reflects the real state of the
**"AstralGuard"** scheduled task, so it stays accurate even if the task is changed from Task
Scheduler directly. It enables and disables the existing task rather than creating one; the
setup creates the task (or `astralguard register` for a portable copy), so its trigger,
principal and restart settings are defined in exactly one place. On Linux the same switch
enables or disables `astralguard.service`.

---

## Pin imbalance warning

The tiers catch a pin that is carrying too much. The imbalance warning catches the cause
earlier: a bad contact raises the resistance of one wire, its current drops, and the load
shifts onto the others, often while every pin is still under the tier 1 threshold.

The rule follows the Thermal Grizzly WireView Pro II (firmware v03): compare the highest and
the lowest pin, and only once a pin carries at least 6 A, since at idle a fraction of an amp
is already a large percentage. It warns when either condition holds for 5 s:

- **Relative:** (highest - lowest) / highest >= **40%**. Thermal Grizzly publishes the 40%
  but not what it is a percentage of; the highest pin is used here, the least trigger-happy
  reading.
- **Absolute:** highest - lowest >= **4.0 A**. This catches a large max delta at high load, where
  the percentage still looks moderate (12 A vs 7.8 A is only 35%).

A wire reading 0 A under load is 100% and always trips. The warning clears once the max delta
falls below 80% of both thresholds, or the load drops under the minimum current, for 5 s.

**What it does** depends on `imbalance_response` (Settings tab: **Warn only / Act like
overcurrent**):

- **`warn`** (default): toast + Event 8001, with all six pin currents in the log. A cable that
  stays bad re-trips on every load change, so the log records each one but the toast repeats
  at most every 10 minutes. The clear is logged, and written to Event Viewer as 8002 if the
  warning was.
- **`act`**: the same thresholds, but a tripped imbalance is handed to the response ladder as a
  tier 1 crossing, so it gets exactly what sustained overcurrent gets: in tiered mode the GPU
  is limited, in shutdown-only mode the machine shuts down, with the flight recorder dump
  and the Event Log entries of a normal tier 1 event.

Why act on it: a pin whose contact has burnt open reads about 0 A, and its current moves
onto the other five. Each carries 20% more current and so 44% more heat (heat goes with the
square of the current), which is how a single bad contact cascades. At full load the tiers
catch that anyway (600 W over five pins is 10 A each); at around 450 W the survivors sit at
7.5 A and only the imbalance check sees it.

How a dead pin plays out in `act` + tiered mode: limiting the GPU drops every pin below the
6 A minimum, so the imbalance can no longer be measured and the limit is released after the
normal hold time. Under load the imbalance comes back, and a re-trigger within the 10-minute
window is a persistent fault, so the second time the guard shuts the machine down.

The risk of `act`: one faulty current-sense channel stuck at 0 A looks exactly like a dead pin.
The 6 A minimum and the sustained window make a false trigger unlikely, not impossible, which
is why `warn` is the default.


---

## GPU sag detection

ROG Astral cards carry a Bosch BMI321 accelerometer (ASUS calls the feature LevelSense). Its
readings are relayed by the same IT8915 chip the pin currents come from, registers
`0x70-0x76`, so the guard reads it over the same read-only NVAPI I2C path. Nothing extra to
install, and GPU Tweak III does not need to be running.

**What it measures.** Two values, both shown as `X.Y° / Z.Z mm`:

- **Sag since calibration:** the angle between gravity now and gravity at calibration. This is
  what the warnings are based on. Works for horizontal cards and vertical mounts alike.
- **Absolute tilt (vs. level):** the uncalibrated reading, how far gravity is from the nearest
  sensor axis. A level horizontal card or a plumb vertical mount reads near 0. It also
  includes any tilt of the desk or floor, which is why warnings use the calibrated value.

The IMU tab shows both, and the tooltip reads e.g. `sag 0.1° / 0.6 mm, abs 0.7° / 4.4 mm`.

**Degrees to millimetres.** Droop at the far end = card length x sin(angle), treating the card
as rigid and pivoting at the slot. The length comes from the ASUS spec for the detected card:

| Subsystem ID | Card | Length | Height | Thickness | Slots | 1° ≈ |
|---|---|---|---|---|---|---|
| `0x89E31043` | ROG Astral RTX 5090 OC | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x8A2E1043` | ROG Astral RTX 5090 OC White | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x89EA1043` | ROG Astral RTX 5090D OC | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x8A5A1043` / `0x8A3C1043` | ROG Astral RTX 5090 BTF OC | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x89DE1043` | ROG Astral RTX 5080 OC | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x89DF1043` | ROG Astral RTX 5080 | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x8A2B1043` | ROG Astral RTX 5080 OC White | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x8A451043` | ROG Astral RTX 5080 OC Hatsune Miku | 357.6 mm | 149.3 mm | 76 mm | 3.8 | 6.2 mm |
| `0x8A611043` | ROG Matrix Platinum RTX 5090 | 370.3 mm | 150.5 mm | 77.3 mm | 3.9 | 6.5 mm |
| `0x89EC1043` | ROG Astral LC RTX 5090 OC (card body; radiator 400 x 120 x 65 mm) | 288.46 mm | 153.7 mm | 48 mm | 2.5 | 5.0 mm |

Figures are from the rog.asus.com spec pages (September 2026). ASUS gives one dimension per
card and does not say whether it includes the I/O bracket. The Matrix row assumes the
id is the Platinum 30th Anniversary card, the only RTX 5090 Matrix ASUS lists. One of the
two BTF ids may be the non-OC BTF, which has not been measured separately. An unknown card
falls back to 357.6 mm. Set `sag_card_length_mm` to override.

**How it samples.** One read every 5 s, taken in the monitor loop right after the pin reads,
and skipped entirely while the GPU is limited. Judgements use the median of the last minute.
G-sensor failures never count against the pin sensor: after a minute of failed reads, sag
detection pauses for 5 minutes and the power guard carries on untouched.

**Calibration.** On first run the guard takes a baseline automatically after one minute, but
only if the card sits within 1.5 deg of level (or plumb). If it is further off it may already
be sagging, so the guard asks you to support the card and calibrate from the dashboard
instead. **Calibrate** (IMU or Settings tab) averages about 10 s of readings. The baseline is stored per card
in `config.ini` and re-taken if a different card is installed.

**What it tells you.** Warn-only, never limits the GPU or shuts down:

| Condition | Response |
|---|---|
| Angle from baseline >= warning angle (1.0 deg) for 60 s | Toast + Event 7001 |
| Angle >= serious angle (2.0 deg) for 60 s | Toast + Event 7002 |
| A jump of >= 1 deg between two samples, still off a minute later | "Orientation changed suddenly" toast + Event 7003. Either the PC was moved (recalibrate), or the card dropped (check the bracket) |
| Daily median up >= 0.3 deg against a day 7-30 days back | "Sag is creeping" toast + Event 7004 |
| Back below 0.7x the angle | Cleared, Event 7006 |
| Manual calibration done | Toast + Event 7005 |
| First run, card more than 1.5 deg off level | No auto-baseline, toast + Event 7007 |

The daily medians are kept in `logs/sag_history.csv`, and flight recorder dumps gain a
`sag_deg` column.

Register layout verified on a ROG Astral RTX 5090 OC (`0x89E31043`). Other Astral models very
likely match (GPU Tweak III uses one code path for all of them); a card that returns
implausible readings simply shows "no G-sensor".

---

## Event Viewer

Logs written to:
```
Event Viewer > Windows Logs > Application
Source: AstralGuard
```

---

## Configuration

### Config File

Settings are saved to `config.ini` in `C:\ProgramData\AstralGuard` (next to `astralguard.exe` for a [portable copy](#portable-use-windows); `/var/lib/astralguard` on Linux). It is created with the defaults on first save; the dashboard writes only the keys it changes and keeps everything else, including comments.

**File format:**
```ini
[Settings]
threshold_amps = 9.5
sustained_seconds_required = 15.0
response_mode = tiered
```

Older versions had a `sensor_backend` setting to choose between the card and HWiNFO. It is
ignored now: the card is the only source.

`response_mode` is `tiered` or `shutdown_only`, described under
[How it responds](#how-it-responds).

**Tier settings.** These are optional. Leave them out and the defaults below apply.
`critical_amps`, `critical_seconds` and `catastrophic_amps` can also be changed from the
[dashboard](#dashboard) at runtime; the rest are only read at startup:

```ini
critical_amps = 13.0
critical_seconds = 3.0
catastrophic_amps = 16.0
mitigation_grace_seconds = 10.0
mitigation_clear_seconds = 60.0
mitigation_min_hold_seconds = 120.0
retrigger_window_seconds = 600.0
```

| Key | Meaning |
|---|---|
| `critical_amps` / `critical_seconds` | Tier 2: current high enough to skip tier 1's 15 s window |
| `catastrophic_amps` | Tier 3: limit and shut down at once, on 2 consecutive samples |
| `mitigation_grace_seconds` | How long a limited GPU has to bring the current down before the guard shuts down instead (tier 2 uses a fixed 5 s) |
| `mitigation_clear_seconds` | How long all pins must stay clear before the limit lifts |
| `mitigation_min_hold_seconds` | Minimum time the limit stays on regardless of how fast the current falls |
| `retrigger_window_seconds` | A tier firing again this soon after a release is a persistent fault, and shuts down |

**Imbalance settings.** Optional, and all changeable from the dashboard:

```ini
imbalance_enabled = True
imbalance_response = warn
imbalance_min_amps = 6.0
imbalance_percent = 40.0
imbalance_delta_amps = 4.0
imbalance_sustained_seconds = 5.0
```

**Sag settings.** Optional, and all changeable from the dashboard:

```ini
sag_enabled = True
sag_warn_degrees = 1.0
sag_serious_degrees = 2.0
sag_sustained_seconds = 60.0
sag_card_length_mm = 0
```

`sag_card_length_mm` only converts degrees to millimetres for display. `0` (the default) takes
the length from the detected card, see the table under
[GPU sag detection](#gpu-sag-detection). `sag_baseline` and
`sag_baseline_subsystem` are written by calibration; delete both to force a new baseline.

### Default Safety Settings

```
THRESHOLD_AMPS = 9.5 A
SUSTAINED_SECONDS_REQUIRED = 15.0 s
CRITICAL_AMPS = 13.0 A / 3.0 s
CATASTROPHIC_AMPS = 16.0 A (2 consecutive samples)
POLL_INTERVAL_SEC = 0.5 s
RESPONSE_MODE = tiered
```

### Changing Settings

**WARNING: Modifying safety thresholds can be dangerous!**

**Increasing threshold values above defaults (9.5 A, 15.0 s, or any of the tier settings) reduces protection and may allow unsafe conditions to persist longer before the guard acts. This can lead to:**
- Connector overheating
- Melting or physical damage to the 12VHPWR connector
- Permanent GPU damage
- Fire risk

**Only modify settings if you fully understand the risks and have a specific, justified reason. Lower values increase safety but may cause false positives during legitimate high-load scenarios.**

**Via the dashboard (Recommended):**
1. Open the dashboard from the tray and go to **Settings**
2. Change the values and press **Apply**
3. A value **above** its default asks for confirmation, because that reduces protection
4. Settings are saved and take effect on the next sample

**Via Config File:**
- Edit `config.ini` directly (restart required for changes to take effect)
- If the file is missing or invalid, defaults are used
- Values must be positive numbers

---

## Development

### Building from source

Install Rust from <https://rustup.rs> with the GNU toolchain (`x86_64-pc-windows-gnu`; no
Visual Studio needed), then run `rust\build.ps1`. It builds a release and copies
`astralguard.exe` to the project folder. `cargo test` (in `rust\`) runs the unit tests: config
parsing, the sensor data format, the imbalance and sag logic, and every rung of the response
ladder against a fake GPU limiter.

The GNU toolchain needs two workarounds, both handled by the build script and
`rust\.cargo\config.toml`: rustup ships no GNU assembler, so LLVM's `dlltool` (a copy of
`llvm-ar` from the `llvm-tools` component) makes the raw-dylib import libraries, and linking
uses `rust-lld`, because GNU ld mislinks those libraries and the program crashes before
`main`. The standard MSVC toolchain works too (that is what CI uses).

### CI and releases

Every push to `master` and every pull request runs [CI](.github/workflows/ci.yml) on
Windows and Linux: `cargo fmt --check`, clippy with warnings as errors (plus a service-only
build on Linux) and the tests. When those pass it builds the three packages
([packages workflow](.github/workflows/packages.yml)): the Windows setup with
[Inno Setup](rust/packaging/windows/astralguard.iss), the `.deb` with
[cargo-deb](https://github.com/kornelski/cargo-deb) (metadata in `rust/Cargo.toml`), and the
Arch package with `makepkg` in an Arch container. Both Linux packages are installed on the
runner as a test. Each run keeps the packages as downloadable artifacts.

To release: bump `version` in `rust/Cargo.toml` (and `pkgver` in the PKGBUILD), commit,
then tag and push:

```
git tag v2.1.0
git push origin v2.1.0
```

The [release workflow](.github/workflows/release.yml) checks that the tag matches the crate
version, builds the same packages and publishes them with a `SHA256SUMS.txt` as a GitHub
release. Installed dashboards pick it up within a day through the [update check](#upgrading).

### Reading the card directly

`astralguard probe` prints live per-pin voltage and current and the accelerometer straight from
the IT8915 chip; it is safe to run next to the guard. The protocol (chip address, register
layout, pin ordering, NVAPI struct, accelerometer registers) is documented in
`rust/src/sensor/mod.rs` and `rust/src/sensor/nvapi.rs`, with the two independent sources it
was verified against; the NVAPI struct layout is checked at compile time. All access is
**read-only**: NVAPI's I2C write function is never bound.

### Previewing the dashboard

`astralguard ui --demo` runs the dashboard against a simulated guard: no hardware access, and
no second guard competing with the installed one. The simulation cycles through normal load,
a pin dropping out and a GPU limit. `--screenshot <file.png> [--tab live|imu|settings|log]
[--theme <name>]` renders one view to a PNG and exits.

---

## Disclaimer & Legal Notice

### IMPORTANT WARNINGS

**This software is provided "AS IS" without warranty of any kind, express or implied.**

1. **Hardware Compatibility**: This utility is designed exclusively for ASUS ROG Astral graphics cards (RTX 5080 / RTX 5090) with per-pin current sensors. It will not function correctly with other GPU models.

2. **Safety Thresholds**: The default safety thresholds (9.5 A per pin, 15.0 s sustained) are based on documented failure modes. **Modifying these values, especially increasing them, can be dangerous and may:**
   - Allow unsafe current conditions to persist
   - Lead to connector overheating, melting, or physical damage
   - Cause permanent GPU damage
   - Create fire hazards
   - Void warranties

3. **No Guarantee of Protection**: This tool is a **supplementary safety measure only**. It does not guarantee protection against all failure modes, and should not be relied upon as the sole safety mechanism. GPU firmware, drivers, and PSU protections should remain active.

4. **System Shutdowns**: This utility will force a system shutdown when thresholds are exceeded and limiting the GPU does not resolve the condition. Ensure you save your work regularly, as unsaved data will be lost during emergency shutdowns.

5. **GPU Limiting**: In its default tiered mode this utility changes your GPU's clock and power
   limit through NVIDIA's management API when it detects an over-current condition. The change
   is temporary and reversed automatically, and neither setting survives a reboot, but while it
   is active the card will run at drastically reduced performance. Choose **Shut down only**
   in the dashboard's Settings tab if you would rather it never touched your GPU settings.

6. **Testing & Validation**: This software has not been extensively tested across all system configurations. Use at your own risk.

### Liability

**THE AUTHORS, CONTRIBUTORS, AND DISTRIBUTORS OF THIS SOFTWARE:**
- **SHALL NOT BE LIABLE** for any direct, indirect, incidental, special, exemplary, or consequential damages (including, but not limited to, procurement of substitute goods or services; loss of use, data, or profits; or business interruption) however caused and on any theory of liability, whether in contract, strict liability, or tort (including negligence or otherwise) arising in any way out of the use of this software, even if advised of the possibility of such damage.

- **SHALL NOT BE RESPONSIBLE** for any hardware damage, data loss, or other consequences resulting from the use or misuse of this software.

- **DO NOT WARRANT** that this software will meet your requirements or that its operation will be uninterrupted or error-free.

**By using this software, you acknowledge that you have read this disclaimer, understand it, and agree to be bound by its terms. You assume full responsibility for any consequences resulting from the use of this software.**

### AI-Assisted Development

This code was developed with the assistance of AI tools (including Claude Code, Cursor AI, and similar assistants). While efforts have been made to ensure correctness, the code is provided without warranty. Users are encouraged to review the code and understand its operation before use.

### Use at Your Own Risk

**This tool is a protective measure only. Use at your own risk.**
