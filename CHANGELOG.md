# Changelog

What changed in each release of AstralGuard, for the people who run it. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). Each release's notes on GitHub are its section
here.

## [Unreleased]

### Fixed

- The Arch release no longer includes an empty `astralguard-debug` package.

## [1.0.0] - 2026-11-11

The first release.

### Protection

- Per-pin monitoring of the 12V-2x6 connector on ASUS ROG Astral cards: all six 12 V pins,
  current and voltage, every 0.5 s, read straight from the card's own monitor chip. No
  HWiNFO or GPU Tweak needed.
- A graduated response in three tiers: a pin over 9.5 A for 15 s or over 13 A for 3 s
  limits the GPU (clock lock and minimum power limit, fully reversible); 16 A, or limiting
  that does not bring the pin back down, shuts the machine down. Every threshold and time
  is configurable, and weakening one asks for confirmation.
- A pin imbalance warning: one pin drifting away from the others is the early sign of a
  bad contact. Warns by default, or acts like an overcurrent if you choose.
- GPU sag detection with the card's accelerometer: a warning when the card tilts away from
  the baseline it was calibrated at, shown in degrees and in millimetres at the end of the
  card.
- A flight recorder: the last minute of per-pin samples is saved whenever the guard acts.
- Built to keep running: protection is its own headless process, a watchdog restarts
  anything that crashes, and nothing in the dashboard can stop protection.

### Dashboard and tray

- A tray icon that shows the state at a glance, and a dashboard with an Overview (a
  thermal-style view of the plug and five minutes of history), Overclock, Sag, Protection,
  Settings and Log tabs. Every tab fits the default window without scrolling.
- Pause protection, with a reminder while it is paused.
- The operating system and NVIDIA driver in the header.
- A daily update check that only tells you about a new release; it never downloads or
  installs anything.

### Overclock (optional)

- A voltage/frequency undervolt (an offset on a voltage range, capped at a target
  clock), the power limit, the memory offset, and XBAR / SYS clock offsets, each applied
  only when ticked.
- XBAR / SYS offsets "only under load" by default, so the card still reaches its idle
  clock.
- Applied after a configurable delay at start, again after a resume from sleep and after
  the guard releases a limit; every value is recorded and put back exactly when unticked.
- Live tiles: GPU power against the limit, temperature, core voltage, the highest pin, and
  the core, XBAR, SYS and memory clocks.
- Every write to the driver's undocumented controls is checked first on each driver
  version, and refused if it does not land exactly as intended.

### Platforms

- Windows 10 / 11: a setup wizard that installs for all users or just you, with a logon
  task that runs the guard elevated without a UAC prompt at every sign-in.
- Linux: `.deb` for Debian 12+ and Ubuntu 22.04+, and an Arch package; a systemd service,
  the tray over StatusNotifierItem, and a scan that finds the card's pin sensor bus (from
  the dashboard, or by itself when you turn that on).
  Tested on CachyOS with KDE Plasma on Wayland and the NVIDIA open kernel module.

[Unreleased]: https://github.com/SzabolcsGergely/AstralGuard/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/SzabolcsGergely/AstralGuard/releases/tag/v1.0.0
