//! Update checker: asks GitHub whether a newer release exists, once shortly after the
//! dashboard starts and then once a day. Notify only: it never downloads or installs.
//!
//! Lives in the UI process on purpose. The protection service makes no network
//! connections at all, and a failing check (offline, rate-limited, GitHub down) can
//! only ever cost a status line here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Deserialize;

use crate::protocol::{APP_NAME, APP_VERSION};

pub const REPO: &str = "SzabolcsGergely/AstralGuard";
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    /// Not checked yet (or checks are off).
    Idle,
    Checking,
    UpToDate {
        checked: jiff::ZonedDateTime,
    },
    Available {
        version: String,
        url: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone)]
pub struct Updates {
    state: Arc<Mutex<UpdateState>>,
    enabled: Arc<AtomicBool>,
    check_now: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// "v2.1.0" / "2.1.0" -> (2, 1, 0). Missing parts count as 0; junk is None.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>());
    let major = it.next()?.ok()?;
    let minor = it.next().unwrap_or(Ok(0)).ok()?;
    let patch = it.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// One check against the GitHub API. Ok(None) means up to date (or no release yet).
fn check() -> Result<Option<Release>, String> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // The OS's own roots, as before ureq 3: native-tls with the platform store.
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(TIMEOUT))
        // 404 and friends come back as a response, so they can be told apart below.
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut response = agent
        .get(&url)
        .header("User-Agent", &format!("{APP_NAME}/{APP_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("offline? ({e})"))?;
    let status = response.status().as_u16();
    answer(status, || response.body_mut().read_to_string().map_err(|e| e.to_string()), APP_VERSION)
}

/// What GitHub's answer means for a dashboard running `current`. The body is read only
/// for a success.
fn answer(
    status: u16,
    body: impl FnOnce() -> Result<String, String>,
    current: &str,
) -> Result<Option<Release>, String> {
    match status {
        200..=299 => {}
        // No release published yet.
        404 => return Ok(None),
        code => return Err(format!("GitHub answered {code}")),
    }
    let release: Release = serde_json::from_str(&body()?).map_err(|e| format!("unexpected answer ({e})"))?;
    if release.draft || release.prerelease || !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    Ok(Some(release))
}

/// A scheduled check: on, past the first wait after start, and a day after the last one.
/// (Check now runs regardless.)
fn due(enabled: bool, since_start: Duration, since_last: Option<Duration>) -> bool {
    enabled && since_start >= FIRST_CHECK_AFTER && since_last.is_none_or(|d| d >= CHECK_EVERY)
}

/// What a check found, for the Settings tab; `notify` once per newly found version (not
/// again every day while it stays the newest).
fn outcome(found: Result<Option<Release>, String>, notified: &mut String, notify: &impl Fn(&str)) -> UpdateState {
    match found {
        Ok(Some(r)) => {
            let version = r.tag_name.trim_start_matches(['v', 'V']).to_string();
            if *notified != version {
                notified.clone_from(&version);
                notify(&version);
            }
            UpdateState::Available { version, url: r.html_url }
        }
        Ok(None) => UpdateState::UpToDate { checked: crate::clock::now() },
        Err(error) => UpdateState::Failed { error },
    }
}

impl Updates {
    /// Start the background checker. `notify` is called once per newly found version.
    pub fn spawn(ctx: egui::Context, enabled: bool, notify: impl Fn(&str) + Send + 'static) -> Updates {
        let u = Updates {
            state: Arc::new(Mutex::new(UpdateState::Idle)),
            enabled: Arc::new(AtomicBool::new(enabled)),
            check_now: Arc::new(AtomicBool::new(false)),
        };
        let worker = u.clone();
        let _ = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            let started = Instant::now();
            let mut last: Option<Instant> = None;
            let mut notified = String::new();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let forced = worker.check_now.swap(false, Ordering::Relaxed);
                let enabled = worker.enabled.load(Ordering::Relaxed);
                if !forced && !due(enabled, started.elapsed(), last.map(|t| t.elapsed())) {
                    continue;
                }
                last = Some(Instant::now());
                worker.set(UpdateState::Checking, &ctx);
                worker.set(outcome(check(), &mut notified, &notify), &ctx);
            }
        });
        u
    }

    fn set(&self, s: UpdateState, ctx: &egui::Context) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = s;
        ctx.request_repaint();
    }

    pub fn state(&self) -> UpdateState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn check_now(&self) {
        self.check_now.store(true, Ordering::Relaxed);
    }
}

/// Open a web page in the user's browser. On Windows through explorer.exe, which hands it
/// to the user's (non-elevated) shell: the dashboard runs elevated, a browser must not.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let _ = std::process::Command::new(std::path::Path::new(&root).join("explorer.exe"))
            .arg(url)
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v2.1.0"), Some((2, 1, 0)));
        assert_eq!(parse_version("2.1"), Some((2, 1, 0)));
        assert_eq!(parse_version("v2.1.3-beta.1"), Some((2, 1, 3)));
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("v2.0.1", "2.0.0"));
        assert!(is_newer("v2.10.0", "2.9.9"));
        assert!(!is_newer("v2.0.0", "2.0.0"));
        assert!(!is_newer("v1.9.9", "2.0.0"));
        assert!(!is_newer("nightly", "2.0.0"));
    }

    fn body(json: &str) -> impl FnOnce() -> Result<String, String> + '_ {
        move || Ok(json.to_string())
    }

    #[test]
    fn githubs_answer() {
        let release = r#"{"tag_name":"v1.2.0","html_url":"https://example/r","draft":false,"prerelease":false}"#;
        let found = answer(200, body(release), "1.1.0").unwrap().expect("newer");
        assert_eq!((found.tag_name.as_str(), found.html_url.as_str()), ("v1.2.0", "https://example/r"));
        assert!(answer(200, body(release), "1.2.0").unwrap().is_none(), "up to date");

        let pre = r#"{"tag_name":"v9.0.0","html_url":"u","prerelease":true}"#;
        assert!(answer(200, body(pre), "1.0.0").unwrap().is_none(), "a pre-release is not offered");
        let draft = r#"{"tag_name":"v9.0.0","html_url":"u","draft":true}"#;
        assert!(answer(200, body(draft), "1.0.0").unwrap().is_none());

        let unread = || -> Result<String, String> { panic!("the body is read only for a success") };
        assert!(answer(404, unread, "1.0.0").unwrap().is_none(), "no release yet");
        assert_eq!(answer(403, unread, "1.0.0").err().unwrap(), "GitHub answered 403");

        let err = answer(200, body("<html>"), "1.0.0").err().unwrap();
        assert!(err.starts_with("unexpected answer"), "{err}");
        let cut = answer(200, || Err("connection reset".into()), "1.0.0").err().unwrap();
        assert_eq!(cut, "connection reset");
    }

    #[test]
    fn checks_wait_for_the_start_and_then_a_day() {
        let (s, h) = (Duration::from_secs(1), Duration::from_secs(3600));
        assert!(!due(true, 29 * s, None), "not in the first half minute");
        assert!(due(true, 30 * s, None));
        assert!(!due(false, 30 * s, None), "turned off");
        assert!(!due(true, 48 * h, Some(23 * h)));
        assert!(due(true, 48 * h, Some(24 * h)), "once a day");
    }

    fn release(tag: &str) -> Result<Option<Release>, String> {
        Ok(Some(Release {
            tag_name: tag.into(),
            html_url: format!("https://example/{tag}"),
            draft: false,
            prerelease: false,
        }))
    }

    #[test]
    fn a_new_version_is_announced_once() {
        let told = std::cell::RefCell::new(Vec::new());
        let notify = |v: &str| told.borrow_mut().push(v.to_string());
        let mut notified = String::new();
        let state = outcome(release("v1.2.0"), &mut notified, &notify);
        assert_eq!(state, UpdateState::Available { version: "1.2.0".into(), url: "https://example/v1.2.0".into() });
        outcome(release("v1.2.0"), &mut notified, &notify);
        outcome(release("V1.2.0"), &mut notified, &notify);
        assert_eq!(*told.borrow(), ["1.2.0"], "not again every day");
        outcome(release("v1.3.0"), &mut notified, &notify);
        assert_eq!(*told.borrow(), ["1.2.0", "1.3.0"], "a newer one is");

        assert!(matches!(outcome(Ok(None), &mut notified, &notify), UpdateState::UpToDate { .. }));
        let failed = outcome(Err("offline? (dns)".into()), &mut notified, &notify);
        assert_eq!(failed, UpdateState::Failed { error: "offline? (dns)".into() });
        assert_eq!(told.borrow().len(), 2);
    }

    #[test]
    fn the_settings_tab_turns_checks_on_off_and_asks_for_one() {
        let u = Updates {
            state: Arc::new(Mutex::new(UpdateState::Idle)),
            enabled: Arc::new(AtomicBool::new(false)),
            check_now: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(u.state(), UpdateState::Idle);
        u.set_enabled(true);
        assert!(u.enabled.load(Ordering::Relaxed));
        u.check_now();
        assert!(u.check_now.load(Ordering::Relaxed));
        let ctx = egui::Context::default();
        u.set(UpdateState::Checking, &ctx);
        assert_eq!(u.clone().state(), UpdateState::Checking, "the worker's clone shares the state");
    }
}

#[cfg(test)]
mod network_tests {
    /// Talks to GitHub: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn github_answers() {
        let r = super::check();
        assert!(r.is_ok(), "{:?}", r.err());
    }
}
