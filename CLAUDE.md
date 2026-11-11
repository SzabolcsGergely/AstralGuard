# AstralGuard: contributor guide

How to build, test and change AstralGuard. It's written for people and for coding agents
such as Claude Code alike. The user-facing documentation is [README.md](README.md); read its
[How it protects](README.md#how-it-protects) and [Architecture](README.md#architecture)
sections first.

AstralGuard is safety software: when it is wrong, a connector melts or a machine shuts
down for nothing. Read [the safety rules](#safety-rules) before changing anything in
`sensor/`, `mitigation.rs` or `service/`.

## Repository layout

```
rust/                     the whole program: one crate, one binary (astralguard)
  src/main.rs             mode dispatch: watchdog | service | ui | probe | status | register | unregister
  src/protocol.rs         everything service and UI say to each other (JSON types), Settings + defaults
  src/config.rs           config.ini read/write (keeps unknown keys and comments), data_dir()
  src/ipc.rs              local socket: named pipe (Windows) / unix socket (Linux)
  src/watchdog.rs         supervises service + ui, restarts crashes (job object / SIGKILL)
  src/logger.rs           astralguard.log (rotates at 2 MB, keeps 5)
  src/crash.rs            panic hook: logs/crash_trace.log
  src/sensor/             IT8915 per-pin + accelerometer reads: mod.rs (ASTRAL_CARDS, parsing, plausibility),
                          nvapi.rs (Windows), i2cdev.rs (Linux),
                          detect.rs (Linux sensor scan: probe --detect / dashboard Scan)
  src/mitigation.rs       NVML clock lock + power limit, and putting them back
  src/nvapi.rs            the one shared NVAPI binding (Windows pin reads, V/F curve calls), one lock;
                          loaded once, never unloaded, Initialize again on every Windows sensor
                          connect (recovers after a driver reset)
  src/overclock/          the optional overclock: curve.rs (math), gpu.rs (OcGpu trait + RealGpu),
                          mod.rs (Overclocker, overclock.active marker, probe), load.rs (the
                          XBAR / SYS load gate), clkctl.rs (XBAR / SYS clock-domain control and its
                          checks; clkctl/windows_rm.rs = the display driver's escape channel,
                          clkctl/linux_rm.rs = ioctls on /dev/nvidiactl)
  src/service/            the protection core; mod.rs = shared state and the IPC requests
    monitor.rs            0.5 s loop: read pins, tier 1 timer, act on the ladder's decision
    response.rs           the response ladder (tiers 1-3, grace, hold, release, retrigger)
    imbalance.rs sag.rs   warnings (sag never acts; imbalance acts only in "act" mode)
    flight.rs             last minute of samples, dumped when the guard acts
    crosscheck.rs         observational comparison with HWiNFO, never decides anything
  src/platform/           windows.rs / linux.rs: elevation, toasts, event log, shutdown, autostart
  src/ui/                 tray + egui dashboard (feature "ui"); live.rs = Overview, overclock.rs = Overclock, imu.rs = Sag,
                          settings.rs = Protection + Settings, log.rs = Log; demo.rs = simulated service;
                          host.rs = Linux tray process that starts the window as `ui --window`
  build.rs                Windows: embeds the icon + version info (writes a .res itself)
  assets/icon/            icon sources (SVG), render.py, the generated .ico / PNGs
  packaging/windows/      Inno Setup script for the setup .exe
  packaging/linux/        systemd unit, sysusers.conf, modules-load.conf (i2c-dev), autostart and
                          launcher .desktop, debian/postinst, arch/PKGBUILD + astralguard.install
  .cargo/config.toml      Windows GNU toolchain linker settings (see below)
  build.ps1               Windows GNU build helper
.github/workflows/        ci.yml (checks), packages.yml (installers), release.yml (tags), scheduled.yml (weekly)
.github/scripts/          safety-rules.sh (the mechanical checks of the safety rules)
.github/ISSUE_TEMPLATE/   bug report, card report, feature request forms; PULL_REQUEST_TEMPLATE.md
docs/screenshots/         README images, rendered with --screenshot
```

## Building

### Linux

Install Rust from <https://rustup.rs>, then the system libraries the tray and window need.

Debian / Ubuntu:

```sh
sudo apt install build-essential pkg-config libssl-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libwayland-dev
```

Arch:

```sh
sudo pacman -S --needed base-devel rust pkgconf openssl libxkbcommon wayland libglvnd
```

Then:

```sh
cd rust
cargo build --release            # target/release/astralguard
cargo build --release --no-default-features   # service only, no tray/window
```

### Windows

Either toolchain works:

- **MSVC (default rustup, what CI uses):** needs the Visual Studio Build Tools (C++
  workload). Then `cd rust` and `cargo build --release`.
- **GNU (no Visual Studio):** `rustup toolchain install stable-x86_64-pc-windows-gnu`,
  `rustup component add llvm-tools`, then run `rust\build.ps1`. Two workarounds are wired
  in, and both are needed:
  - rustup's GNU toolchain ships no assembler, so the `windows` crate's raw-dylib import
    libraries are made by LLVM's dlltool. `build.ps1` copies `llvm-ar.exe` to
    `%USERPROFILE%\.cargo\dlltool\dlltool.exe` and puts it on PATH. To build without the
    script, put that folder on PATH yourself.
  - GNU ld mislinks those import libraries (an import like `GetProcessHeap` stays unbound
    and the exe crashes before `main`), so `rust/.cargo/config.toml` links with
    `rust-lld`.

`build.ps1` also copies the exe to the repository root (a portable install: data next to
it). That copy fails while a guard is running from there. Stop it first.

No resource compiler is needed on either toolchain: `build.rs` writes the `.res` file
(icon + version info) itself, and both link.exe and rust-lld accept it.

## Running it while developing

- **No hardware needed:** `astralguard ui --demo` runs the dashboard against a simulated
  service that cycles through idle, gaming load, an imbalance and a tier 1 trip (90 s).
  `--phase <s>` starts at a point in the cycle (52 = imbalance, 67 = GPU limited),
  `--offline` simulates a missing service, `--no-sensor` a card whose pin monitor reads
  nothing until Scan is pressed.
- **Screenshots:** `astralguard ui --demo --phase 35 --screenshot out.png --tab live|overclock|imu|protection|settings|log
  [--theme <name>] [--size 800x820] [--scale 1.5] [--confirm pause|exit|reset|calibrate|warnings|scan|scan-result]
  [--check-layout]` renders one frame and exits; `--check-layout` makes it exit 3 when the
  tab would scroll. `docs/screenshots/` is made this way.
- **Real card:** `astralguard probe` reads the chip and prints pins, volts and tilt. It is
  read-only and safe next to a running guard. A real guard (`astralguard`) needs elevation
  (Windows) or root (Linux) to limit the GPU; without it, it runs shutdown-only and says so.
- Only one guard runs per session. There are three locks: the watchdog's (a second start opens
  the dashboard instead), the service's (two guards would mean two shutdown commands) and
  the UI's. A just-stopped guard can take seconds to exit, so the service and the
  watchdog-started UI wait up to 15 s for a lock before giving up. Don't shorten that:
  giving up early can leave no guard running. If a guard is installed, stop it (Task
  Scheduler task `AstralGuard`, or `systemctl stop astralguard`) before running your build,
  and start it again afterwards.
- Windows elevation: a non-elevated `astralguard` starts the guard through this user's
  logon task (no prompt), or relaunches itself with a UAC prompt. Windows lets only an
  elevated caller run the all-users task (Users group, highest privileges) on demand.

## Checks (what CI runs)

Run these from `rust/` before pushing. CI runs them on both Windows and Linux and treats
warnings as errors:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings   # service-only build
cargo test
```

- Code behind `#[cfg(windows)]` is dead code on Linux and vice versa: a field or function
  used only on one OS needs `#[cfg(...)]` or `#[cfg_attr(not(windows), allow(dead_code))]`,
  or the other OS's clippy fails.
- `cargo test -- --include-ignored` also runs the tests that need the network (the update
  check).
- Besides those, CI (`.github/workflows/ci.yml`) runs:
  - **Safety rules** (`bash .github/scripts/safety-rules.sh`, runs locally too): the
    mechanical half of the [safety rules](#safety-rules). No I2C write bound (1), no network
    or TLS crate in the service-only build and no network code outside `src/ui/` (2),
    shutdown by absolute path (4), the V/F write only from `overclock/` (9), and only the
    checked commands on the XBAR / SYS channel (10). Changing a check goes with changing its rule.
  - **cargo deny** (`rust/deny.toml`): security advisories, licences (a new licence is a
    decision), crate sources.
  - **Screenshots and layout**: every tab rendered from the demo on Linux (software OpenGL,
    a virtual display), at phases 35 and 67, with `--check-layout`, which exits 3 when a tab
    would scroll. The PNGs are the run's `screenshots` artifact: look there for a UI change.
  - **Minimum Rust**: `cargo check` on the `rust-version` in `Cargo.toml` (1.95, what egui
    0.37 needs). Raise it on purpose, not by using a newer API by accident.
  - **Windows GNU**: `build.ps1` and a start of the exe, so the GNU-specific link setup
    keeps working.
  - **Coverage**: `cargo llvm-cov` on Linux; the run summary has a per-file table and the
    HTML report is the `coverage` artifact. The decision code (the ladder, imbalance, sag,
    the overclock's math, apply / restore and checks, config parsing) has per-file floors in
    `.github/coverage-floors.txt`; raise one when coverage rises, never lower one to let a
    change through. Hardware, FFI and GUI code is reported only: it cannot run in CI.
  - Everything with `--locked` (`Cargo.lock` must be committed and current), on a pinned
    toolchain (`RUST_TOOLCHAIN` in ci.yml and packages.yml); jobs and test steps have
    timeouts, so a deadlocked test fails instead of hanging.
  - **Latest stable**: clippy and the tests on the newest stable Rust, Windows and Linux, on
    every push. Informational (`continue-on-error`), never blocks and the packages don't wait
    for it: when it goes red, fix the new lint and raise the pin on purpose.
  - `.github/workflows/scheduled.yml` runs weekly: the latest stable and beta (new lints
    before they reach the pin), the network tests, and the advisory check. Dependabot
    (`.github/dependabot.yml`) opens weekly update PRs for crates and actions.
- The ladder is tested end to end against a fake limiter (`service/response.rs`):
  `evaluate()` returns a shutdown reason instead of shutting down. Any change to the
  response logic comes with a test there.

Formatting: `rustfmt.toml` (max width 120). Comments explain *why*, not what; keep the
existing tone of the module doc comments.

## Safety rules

These are not style preferences. A change that breaks one gets rejected.

1. **I2C is read-only.** Every transaction to the IT8915 (device `0x2B`) is a read: "set
   register pointer, read N bytes", and the sensor scan's SMBus receive byte. Never write a
   data register, never probe other I2C addresses: a write to the wrong device on a GPU bus
   can permanently damage hardware. `NvAPI_I2CWriteEx` is deliberately not bound. Don't bind
   it. The one place that touches more than one bus is the sensor scan (`sensor/detect.rs`):
   only on a person's request or with `sensor_auto_scan` on (off by default; then at most
   once each time the pins stop reading, never at every start), only the Astral's own
   adapters (PCI vendor `0x10DE`, adapters named `NVIDIA i2c adapter …`), only address
   `0x2B`, an SMBus receive byte (no pointer write) first, and the pointer+read only where
   that was acknowledged. The service refuses a scan while a sensor is connected, and the
   scan holds the bus lock so the monitor cannot connect meanwhile. On Linux, reads go
   through `I2C_SMBUS`: the NVIDIA driver returns zeros for the same read through
   `I2C_RDWR`.
2. **The service makes no network connections.** Anything networked (the update check)
   lives in the UI process.
3. **Nothing in the UI can stop protection.** The UI is a client of the service over IPC.
   It must keep working, and keep retrying, while the service is down, and a UI crash or
   hang must not affect the service. Don't move protection logic into the UI.
4. **The shutdown decision stays testable.** `TieredResponse::evaluate()` returns a reason;
   only the monitor loop acts on it. Shutdown uses absolute paths
   (`%SystemRoot%\System32\shutdown.exe`, `/usr/bin/systemctl`, falling back to
   `/sbin/shutdown -P now`), never PATH.
5. **GPU limiting is reversible.** `mitigation.rs` records the clock and power limit before
   changing them and restores exactly those. The `mitigation.active` marker file covers a
   crash while limited. In shutdown-only mode the protection path never touches NVML. The
   overclock is the user's separate, opt-in use of the GPU with its own NVML binding, and
   it records and restores its own values the same way (`overclock.active`).
6. **Defaults are safety decisions.** Changing a threshold or timing default needs a reason
   in the commit message. The dashboard asks for confirmation when a change weakens
   protection; keep that.
7. **Config stays compatible.** `config.ini` keeps its keys from version to version.
   A bad value falls back to its own default; unknown keys and comments survive a save.
8. **Verified facts are marked.** Sensor constants and NVML behaviour carry
   `VERIFIED (date, card)` notes; anything not confirmed on hardware is marked
   `UNVERIFIED`, like which physical socket each Pin number is. Keep it that way.
9. **NVAPI's V/F calls are the overclock's only.** The NVAPI V/F curve functions are
   undocumented. They are used only in `overclock/`, only behind the compatibility check
   (write 15 MHz to one point, because the driver does not apply a 5 MHz offset, read the whole curve back, restore), and a per-point offset
   beyond ±1000 MHz is refused. NVAPI is loaded once and never unloaded, Initialize runs
   again on every Windows sensor connect (so the binding recovers after a driver reset),
   and every call goes through one lock. So a hung driver call delays the pin reads too;
   that is why the overclock calls NVAPI only when it applies (Apply now, at start, after a
   limit is released, on resume), on an untick and on status requests. Rule 1
   is unchanged: nothing here writes to I2C.
10. **The XBAR / SYS clock control writes only behind its checks.** The driver's clock-domain
    control (`overclock/clkctl.rs`; on Windows through the display driver's escape channel, on
    Linux through ioctls on `/dev/nvidiactl`) is
    undocumented and used only by the overclock. Every reply is checked against the layout
    and refused when anything differs; the domains are found by their id, never by a fixed
    index; only the clock-domain SET_CONTROL is ever sent as a write, carrying one domain's
    offset and nothing else. Before the first write on a driver version the write check runs
    (+15 MHz on XBAR, the whole control block of every domain read back, undone, compared byte
    for byte); a wrong landing fails that driver for good (`oc_clk_failed_driver`). Every
    write is read back. Don't add another command to this channel without the same checks.

## Common pitfalls

Things that have gone wrong before, and how to avoid them.

- **Screenshots from a stale binary.** `rust/target/release/astralguard.exe` is not always
  the newest build: `build.ps1` and other target dirs leave the newest exe elsewhere (the
  repository root). Build first, then render from the exe you just built, and check the tab
  bar in the image matches the code. Regenerate `docs/screenshots/` with every UI change:

  ```sh
  astralguard ui --demo --phase 35 --screenshot docs/screenshots/live.png --tab live
  astralguard ui --demo --phase 67 --screenshot docs/screenshots/protecting.png --tab live
  # and overclock, imu, protection, settings at --phase 35
  ```
- **A tab that scrolls at the default size.** Every tab must fit the default window
  (800x820) without scrolling, also with a banner showing (`--phase 67` shows one). CI's
  screenshot job fails when one does (`--check-layout`). The
  last chart on a tab takes what is left (`ui.available_height()`, clamped) instead of a
  fixed height; the Overclock curve measures the rest of the tab on the frame before.
  Check new content with a screenshot at the default size and at `--phase 67`.
- **Protection settings in more than one place.** `SettingsView` is one form: one diff, one
  dirty flag, and `sync` skips reloading while anything is edited; **Reset to defaults**
  resets every protection group together. Protection values are edited only on the
  Protection tab. Other tabs show a read-only summary with **Edit…**.
- **Tab keys are stored.** `Tab::key()` is what `ui.json` (`last_tab`) and `--tab` use.
  Rename a tab's label, never its key; add an alias in `Tab::parse` for an old name.
- **Polling the GPU faster.** The Overclock tab's status request reads the whole V/F curve
  over NVAPI, which on Windows shares its lock with the pin reads (rule 9). Keep it at 2 s;
  a faster live value needs its own NVML-only request.
- **A new `OcStatus` / `Live` field or request without the demo.** `ui/demo.rs` answers
  everything the dashboard asks; forget it and `--demo` and the screenshots show dashes.
- **Overclock write order on Windows.** An NVML memory offset write resets every V/F curve
  delta to 0 (driver 621.14). `Overclocker::run` restores and applies the curve after the
  memory step for that reason; keep that order and its tests.
- **`astralguard status` says "Access is denied".** The named pipe accepts only elevated
  callers; run it from an elevated shell. That is by design (an ordinary program cannot
  pause the guard), not a bug.
- **Coverage on the Windows GNU toolchain.** It ships no profiler runtime, so
  `cargo llvm-cov` fails there. Measure coverage on Linux or with MSVC.

## Changing things

- **Service ↔ UI protocol:** both sides live in `protocol.rs` and ship in the same binary,
  so there is no version skew to handle. Add a `Request`/`Response` variant, handle it in
  `service/mod.rs`, call it from `ui/backend.rs`, and teach `ui/demo.rs` to answer it.
- **A new setting:** add it to `Settings`, its default, `SettingsPatch`, the check/merge in
  `config.rs` (with validation), the Protection tab (`ui/settings.rs`), and the README's configuration table.
- **A new supported card:** add its PCI subsystem ID and length to `ASTRAL_CARDS` in
  `sensor/mod.rs`, and to the README table. Only Astral cards have the per-pin monitor.
- **Logging:** `logger::{info, warn, error, critical}` to `astralguard.log`; user-visible
  events also go through `shared.event(kind, id, msg)` (Event Log) and `shared.notify`
  (toast). Event IDs are listed in the README; keep the table in sync.

## Icon

The source is `rust/assets/icon/astralguard.svg` (32 px and below use
`astralguard-small.svg`). After editing either, regenerate the `.ico` and PNGs, which are
committed so a normal build needs neither tool:

```sh
cargo install resvg
python rust/assets/icon/render.py
```

The `.ico` is embedded by `build.rs` and used by the installer. The 64 px PNG becomes the
window icon, with a status dot added at runtime. The 256 px PNG and the SVG go into the
Linux packages (`hicolor`).

## Packaging and releases

`.github/workflows/packages.yml` builds all installers; CI runs it on every push (the
results are downloadable run artifacts), and `release.yml` runs it for tags.

| Package | Built by | Notes |
|---|---|---|
| `AstralGuard-<v>-setup.exe` | Inno Setup, `rust/packaging/windows/astralguard.iss` | Asks "all users" (Program Files, `astralguard register --all-users`: a task for the Users group) or "just me" (user profile, `astralguard register` via one UAC prompt) |
| `astralguard_<v>-1_amd64.deb` | cargo-deb, `[package.metadata.deb]` in `rust/Cargo.toml` | Built in a `debian:bookworm` container so it runs on Debian 12+ / Ubuntu 22.04+; install-tested on Debian 12 and Ubuntu 22.04, 24.04 and latest |
| `astralguard-<v>-1-x86_64.pkg.tar.zst` | `makepkg`, `rust/packaging/linux/arch/PKGBUILD` | Built and install-tested in an `archlinux` container |

To cut a release:

1. Bump `version` in `rust/Cargo.toml` (and `pkgver` in the PKGBUILD), and commit.
2. Tag and push:

   ```sh
   git tag v2.1.0
   git push origin v2.1.0
   ```

The release workflow checks that the tag matches the crate version, builds and tests the
packages, and publishes them with `SHA256SUMS.txt`. Installed dashboards see the new
release within a day.

## Conventions

- Branch: `master`. Keep commits focused, with a message that says why.
- Platform-specific code goes in `platform/` or behind `#[cfg]`; shared code must build on
  both OSes.
- User-facing wording: the status words are **Safe / Warning / Protecting / Danger /
  Unprotected / Unknown** (`Status::text`). Use them consistently in the UI, logs and docs.
- The app supports ASUS ROG Astral cards only, on purpose: no other card has per-pin
  sensing, and there is no fallback sensor source.
