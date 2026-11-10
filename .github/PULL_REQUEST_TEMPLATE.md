## What and why

<!-- What this changes, and the reason. A changed threshold or timing default needs its reason here (safety rule 6). -->

## Tested on

<!-- OS, card, driver; or "demo only" / "tests only". Hardware-facing changes say what was run on a real card. -->

## Checklist

- [ ] `cargo fmt --check`, both `cargo clippy` runs and `cargo test` pass (see [CLAUDE.md](https://github.com/SzabolcsGergely/AstralGuard/blob/master/CLAUDE.md#checks-what-ci-runs))
- [ ] The [safety rules](https://github.com/SzabolcsGergely/AstralGuard/blob/master/CLAUDE.md#safety-rules) still hold; I2C stays read-only, the service makes no network connections
- [ ] A change to the response ladder comes with a test in `service/response.rs`
- [ ] Hardware facts are marked `VERIFIED (date, card)` or `UNVERIFIED`
- [ ] A new setting is in `Settings`, `SettingsPatch`, `config.rs`, the Protection tab and the README's configuration table
- [ ] A UI change: every tab still fits the default window without scrolling, and `docs/screenshots/` is regenerated
- [ ] README and CLAUDE.md describe the code as it now is
