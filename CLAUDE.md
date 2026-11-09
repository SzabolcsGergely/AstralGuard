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
  src/sensor/             IT8915 per-pin + accelerometer reads: nvapi.rs (Windows), i2cdev.rs (Linux)
  src/mitigation.rs       NVML clock lock + power limit, and putting them back
  src/service/            the protection core
    monitor.rs            0.5 s loop: read pins, tier 1 timer, act on the ladder's decision
    response.rs           the response ladder (tiers 1-3, grace, hold, release, retrigger)
    imbalance.rs sag.rs   warnings (sag never acts; imbalance acts only in "act" mode)
    flight.rs             last minute of samples, dumped when the guard acts
    crosscheck.rs         observational comparison with HWiNFO, never decides anything
  src/platform/           windows.rs / linux.rs: elevation, toasts, event log, shutdown, autostart
  src/ui/                 tray + egui dashboard (feature "ui"); demo.rs = simulated service
  build.rs                Windows: embeds the icon + version info (writes a .res itself)
  assets/icon/            icon sources (SVG), render.py, the generated .ico / PNGs
  packaging/windows/      Inno Setup script for the setup .exe
  packaging/linux/        systemd unit, autostart entry, debian/postinst, arch/PKGBUILD
  .cargo/config.toml      Windows GNU toolchain linker settings (see below)
  build.ps1               Windows GNU build helper
.github/workflows/        ci.yml (checks), packages.yml (installers), release.yml (tags)
docs/screenshots/         README images, rendered with --screenshot
```

## Building

### Linux

Install Rust from <https://rustup.rs>, then the system libraries the tray and window need.

Debian / Ubuntu:

```sh
sudo apt install build-essential pkg-config libssl-dev libgtk-3-dev libxdo-dev \
  libayatana-appindicator3-dev libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libwayland-dev
```

Arch:

```sh
sudo pacman -S --needed base-devel rust pkgconf gtk3 libayatana-appindicator xdotool openssl
```

Then:

```sh
cd rust
cargo build --release            # target/release/astralguard
cargo build --release --no-default-features   # service only, no tray/window/GTK
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
  `--offline` simulates a missing service.
- **Screenshots:** `astralguard ui --demo --phase 35 --screenshot out.png --tab live|imu|settings|log
  [--theme <name>] [--size 800x820] [--scale 1.5] [--confirm pause|exit|reset|calibrate|warnings]`
  renders one frame and exits. `docs/screenshots/` is made this way.
- **Real card:** `astralguard probe` reads the chip and prints pins, volts and tilt. It is
  read-only and safe next to a running guard. A real guard (`astralguard`) needs elevation
  (Windows) or root (Linux) to limit the GPU; without it, it runs shutdown-only and says so.
- Only one guard runs per session. There are three locks: the watchdog's (a second start opens
  the dashboard instead), the service's (two guards would mean two shutdown commands) and
  the UI's. A just-stopped guard can take seconds to exit, so the service and the
  watchdog-started UI wait up to 15 s for a lock before giving up. Don't shorten that:
  giving up early once left no guard running. If a guard is installed, stop it (Task
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
- The ladder is tested end to end against a fake limiter (`service/response.rs`):
  `evaluate()` returns a shutdown reason instead of shutting down. Any change to the
  response logic comes with a test there.

Formatting: `rustfmt.toml` (max width 120). Comments explain *why*, not what; keep the
existing tone of the module doc comments.

## Safety rules

These are not style preferences. A change that breaks one gets rejected.

1. **I2C is read-only.** Every transaction to the IT8915 (device `0x2B`) is "set register
   pointer, read N bytes". Never write a data register, never probe other I2C addresses or
   buses: a write to the wrong device on a GPU bus can permanently damage hardware.
   `NvAPI_I2CWriteEx` is deliberately not bound. Don't bind it.
2. **The service makes no network connections.** Anything networked (the update check)
   lives in the UI process.
3. **Nothing in the UI can stop protection.** The UI is a client of the service over IPC.
   It must keep working, and keep retrying, while the service is down, and a UI crash or
   hang must not affect the service. Don't move protection logic into the UI.
4. **The shutdown decision stays testable.** `TieredResponse::evaluate()` returns a reason;
   only the monitor loop acts on it. Shutdown uses absolute paths
   (`%SystemRoot%\System32\shutdown.exe`, `/usr/bin/systemctl`), never PATH.
5. **GPU limiting is reversible.** `mitigation.rs` records the clock and power limit before
   changing them and restores exactly those. The `mitigation.active` marker file covers a
   crash while limited. Shutdown-only mode never touches NVML.
6. **Defaults are safety decisions.** Changing a threshold or timing default needs a reason
   in the commit message. The dashboard asks for confirmation when a change weakens
   protection; keep that.
7. **Config stays compatible.** `config.ini` keeps the original Python version's keys. A
   bad value falls back to its own default; unknown keys and comments survive a save.
8. **Verified facts are marked.** Sensor constants and NVML behaviour carry
   `VERIFIED (date, card)` notes; anything not confirmed on hardware is marked
   `UNVERIFIED`, like the Linux I2C adapter mapping. Keep it that way.

## Changing things

- **Service ↔ UI protocol:** both sides live in `protocol.rs` and ship in the same binary,
  so there is no version skew to handle. Add a `Request`/`Response` variant, handle it in
  `service/mod.rs`, call it from `ui/backend.rs`, and teach `ui/demo.rs` to answer it.
- **A new setting:** add it to `Settings`, its default, `SettingsPatch`, the check/merge in
  `config.rs` (with validation), the Settings tab, and the README's configuration table.
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
| `astralguard_<v>-1_amd64.deb` | cargo-deb, `[package.metadata.deb]` in `rust/Cargo.toml` | Built in a `debian:bookworm` container so it runs on Debian 12+ / Ubuntu 22.04+; install-tested on four distributions |
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
