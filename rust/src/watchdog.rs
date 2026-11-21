//! `astralguard` / `astralguard watchdog`: runs the service and the UI and restarts either if
//! it dies.
//!
//! The scheduled task (Windows) starts this, not the service: Task Scheduler's "restart on
//! failure" only covers a task that fails to start, not a program that crashes later.
//! The service exiting cleanly (0: exit from the dashboard, a shutdown it fired, or
//! another guard already running) ends everything. Any other exit is a crash: restart
//! after a delay that grows while it keeps failing, and tell the service why
//! (GUARD_WATCHDOG_RESTART) so it can log and notify. A UI that exits cleanly stays
//! closed; a UI that crashes is restarted the same way.
//!
//! Both children live in a job object that closes with this process (Windows) or get
//! SIGKILL when it dies (Linux), so stopping the task also stops the guard.
//!
//! No hardware access here: this is the process that must not crash.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use crate::logger;

/// Seconds before each restart; stays at the last value while it keeps failing.
const BACKOFF_SEC: [u64; 4] = [5, 10, 30, 60];
/// A run at least this long counts as healthy and resets the backoff.
const STABLE_RUN_SEC: u64 = 600;

struct Supervised {
    mode: &'static str,
    child: Option<Child>,
    started: Instant,
    failures: usize,
    restart_at: Option<Instant>,
    restart_reason: Option<String>,
}

impl Supervised {
    fn new(mode: &'static str) -> Self {
        Supervised {
            mode,
            child: None,
            started: Instant::now(),
            failures: 0,
            restart_at: Some(Instant::now()),
            restart_reason: None,
        }
    }
}

/// What the watchdog does when a child exits.
#[derive(Debug, PartialEq)]
enum AfterExit {
    /// The service stopped on purpose (exit, a shutdown it fired, another guard running).
    StopEverything,
    /// The dashboard was closed by its user: the service keeps running.
    StayClosed,
    /// Anything else is a crash.
    Restart,
}

fn after_exit(mode: &str, code: Option<i32>) -> AfterExit {
    match (mode, code) {
        ("service", Some(0)) => AfterExit::StopEverything,
        (_, Some(0)) => AfterExit::StayClosed,
        _ => AfterExit::Restart,
    }
}

pub fn describe_exit(code: Option<i32>) -> String {
    let Some(code) = code else { return "killed by a signal".into() };
    let c = code as u32;
    let name = match c {
        0xC000_0005 => Some("access violation"),
        0xC000_00FD => Some("stack overflow"),
        0xC000_0409 => Some("stack buffer overrun"),
        0xC000_0374 => Some("heap corruption"),
        101 => Some("panic"),
        _ => None,
    };
    format!("exit code 0x{c:08X}{}", name.map(|n| format!(" ({n})")).unwrap_or_default())
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("astralguard"))
}

pub fn run(args: &[String]) -> i32 {
    logger::init(&crate::config::log_dir().join("watchdog.log"), logger::Level::Info);
    crate::crash::install(&crate::config::log_dir());
    let with_ui = cfg!(feature = "ui") && !args.iter().any(|a| a == "--no-ui");
    // Started again (Start menu, desktop shortcut) while the guard runs: open the dashboard
    // instead. The UI shows the running window, or starts one if it was closed.
    #[cfg(windows)]
    let open_dashboard = || {
        if with_ui {
            let _ = Command::new(exe()).args(["ui", "--show"]).spawn();
        }
        0
    };
    #[cfg(windows)]
    if crate::platform::watchdog_running() {
        return open_dashboard();
    }
    // Started without elevation (the Start menu shortcut) while the guard is not running:
    // start it elevated, through this user's logon task (no prompt) or else with a UAC
    // prompt. Declined: carry on without elevation, which means shutdown-only.
    #[cfg(windows)]
    if !crate::platform::is_elevated() {
        if crate::platform::start_task() {
            logger::info("Not elevated: started the guard through its logon task instead.");
            return 0;
        }
        if crate::platform::relaunch_elevated() {
            logger::info("Not elevated: started the guard again with administrator rights.");
            return 0;
        }
        logger::warn("Not elevated and elevation declined: running shutdown-only.");
    }
    // Taken only now: the elevated copy started above must not find it held by this one.
    #[cfg(windows)]
    if !crate::platform::acquire_watchdog_lock() {
        return open_dashboard();
    }
    let job = job::create();
    logger::info(format!(
        "Watchdog started (pid {}); supervising the service{}.",
        std::process::id(),
        if with_ui { " and the UI" } else { "" }
    ));

    let mut service = Supervised::new("service");
    let mut ui = with_ui.then(|| Supervised::new("ui"));

    while supervise(&mut service, ui.as_mut(), &mut |sup| start_with(sup, &job, command(&exe(), sup))) {
        std::thread::sleep(Duration::from_millis(200));
    }
    0
}

/// One pass: start what is due, then act on whatever exited. False once the service
/// stopped on purpose: the UI is killed and the watchdog ends too.
fn supervise(
    service: &mut Supervised,
    mut ui: Option<&mut Supervised>,
    start: &mut dyn FnMut(&mut Supervised),
) -> bool {
    for sup in std::iter::once(&mut *service).chain(ui.as_deref_mut()) {
        if sup.child.is_none() && sup.restart_at.is_some_and(|t| Instant::now() >= t) {
            start(sup);
        }
    }

    if let Some(code) = poll(service) {
        if after_exit(service.mode, code) == AfterExit::StopEverything {
            logger::info(format!(
                "Service exited cleanly after {:.1} min; watchdog stopping.",
                service.started.elapsed().as_secs_f64() / 60.0
            ));
            if let Some(ui) = ui.and_then(|u| u.child.as_mut()) {
                let _ = ui.kill();
            }
            return false;
        }
        schedule_restart(service, code);
    }
    if let Some(sup) = ui {
        if let Some(code) = poll(sup) {
            if after_exit(sup.mode, code) == AfterExit::StayClosed {
                logger::info("UI closed; the service keeps running.");
                sup.restart_at = None;
            } else {
                schedule_restart(sup, code);
            }
        }
    }
    true
}

/// `program <mode>`, told it runs under the watchdog and, after a crash, why it was restarted.
fn command(program: &std::path::Path, sup: &Supervised) -> Command {
    let mut cmd = Command::new(program);
    cmd.arg(sup.mode).env("GUARD_WATCHDOG", "1");
    match &sup.restart_reason {
        Some(r) => {
            cmd.env("GUARD_WATCHDOG_RESTART", r);
        }
        None => {
            cmd.env_remove("GUARD_WATCHDOG_RESTART");
        }
    }
    cmd
}

/// Start `cmd` as `sup`'s child, in the kill-on-close job; a failed start is retried like a crash.
fn start_with(sup: &mut Supervised, job: &job::Job, mut cmd: Command) {
    job::prepare(&mut cmd);
    sup.started = Instant::now();
    sup.restart_at = None;
    match cmd.spawn() {
        Ok(child) => {
            if !job.assign(&child) {
                logger::warn(format!(
                    "{} not placed in the kill-on-close job; it would outlive a killed watchdog.",
                    sup.mode
                ));
            }
            sup.child = Some(child);
        }
        Err(e) => {
            logger::error(format!("Could not start the {}: {e}", sup.mode));
            schedule_restart(sup, Some(-1));
        }
    }
}

/// Some(exit code) once the child has exited.
fn poll(sup: &mut Supervised) -> Option<Option<i32>> {
    let child = sup.child.as_mut()?;
    match child.try_wait() {
        Ok(Some(status)) => {
            sup.child = None;
            Some(status.code())
        }
        _ => None,
    }
}

fn schedule_restart(sup: &mut Supervised, code: Option<i32>) {
    let ran = sup.started.elapsed();
    if ran.as_secs() >= STABLE_RUN_SEC {
        sup.failures = 0;
    }
    let delay = BACKOFF_SEC[sup.failures.min(BACKOFF_SEC.len() - 1)];
    sup.failures += 1;
    let reason = format!("{} after {:.1} h", describe_exit(code), ran.as_secs_f64() / 3600.0);
    logger::error(format!("{} died: {reason}. Restarting in {delay} s.", sup.mode));
    sup.restart_reason = Some(reason);
    sup.restart_at = Some(Instant::now() + Duration::from_secs(delay));
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Kept open for the watchdog's lifetime; closing it (however the watchdog ends)
    /// kills both children.
    pub struct Job(Option<HANDLE>);

    pub fn create() -> Job {
        unsafe {
            let Ok(h) = CreateJobObjectW(None, None) else { return Job(None) };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_ok();
            Job(ok.then_some(h))
        }
    }

    impl Job {
        pub fn assign(&self, child: &Child) -> bool {
            let Some(job) = self.0 else { return false };
            unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).is_ok() }
        }
    }

    pub fn prepare(_cmd: &mut Command) {}
}

#[cfg(unix)]
mod job {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    pub struct Job;

    pub fn create() -> Job {
        Job
    }

    impl Job {
        pub fn assign(&self, _child: &Child) -> bool {
            true
        }
    }

    /// The child gets SIGKILL when the watchdog dies.
    pub fn prepare(cmd: &mut Command) {
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_service_exit_stops_everything_and_anything_else_restarts() {
        assert_eq!(after_exit("service", Some(0)), AfterExit::StopEverything);
        assert_eq!(after_exit("service", Some(1)), AfterExit::Restart);
        assert_eq!(after_exit("service", Some(101)), AfterExit::Restart, "a panic is a crash");
        assert_eq!(after_exit("service", None), AfterExit::Restart, "killed by a signal");
        assert_eq!(after_exit("ui", Some(0)), AfterExit::StayClosed, "the dashboard closed by its user");
        assert_eq!(after_exit("ui", Some(101)), AfterExit::Restart);
        assert_eq!(after_exit("ui", None), AfterExit::Restart);
    }

    /// The restart delay `schedule_restart` set, in whole seconds.
    fn delay(sup: &Supervised) -> u64 {
        let d = sup.restart_at.unwrap().saturating_duration_since(Instant::now());
        (d.as_secs_f64() + 0.5) as u64
    }

    #[test]
    fn restarts_back_off_while_it_keeps_failing() {
        let mut sup = Supervised::new("service");
        let mut delays = Vec::new();
        for _ in 0..6 {
            sup.started = Instant::now(); // crashed straight after starting
            schedule_restart(&mut sup, Some(101));
            delays.push(delay(&sup));
        }
        assert_eq!(delays, [5, 10, 30, 60, 60, 60], "stays at the last value, never gives up");
    }

    #[test]
    fn a_long_healthy_run_resets_the_backoff() {
        let mut sup = Supervised::new("service");
        for _ in 0..3 {
            schedule_restart(&mut sup, Some(1));
        }
        assert_eq!(delay(&sup), 30);
        let Some(long_ago) = Instant::now().checked_sub(Duration::from_secs(STABLE_RUN_SEC)) else {
            return; // a clock this young cannot go back 10 minutes; nothing to test
        };
        sup.started = long_ago;
        schedule_restart(&mut sup, Some(1));
        assert_eq!(delay(&sup), 5, "a crash after 10 minutes of running starts the backoff over");
    }

    #[test]
    fn the_restart_tells_the_service_why() {
        let mut sup = Supervised::new("service");
        schedule_restart(&mut sup, Some(0xC000_0005u32 as i32));
        let why = sup.restart_reason.as_deref().unwrap();
        assert!(why.starts_with("exit code 0xC0000005 (access violation) after 0.0 h"), "{why}");
    }

    #[test]
    fn exit_codes_read_like_their_cause() {
        assert_eq!(describe_exit(None), "killed by a signal");
        assert_eq!(describe_exit(Some(101)), "exit code 0x00000065 (panic)");
        assert_eq!(describe_exit(Some(0xC000_00FDu32 as i32)), "exit code 0xC00000FD (stack overflow)");
        assert_eq!(describe_exit(Some(-1)), "exit code 0xFFFFFFFF", "could not start: no name");
        assert_eq!(describe_exit(Some(3)), "exit code 0x00000003");
    }

    fn shell(script: &str) -> Command {
        let mut c = if cfg!(windows) { Command::new("cmd") } else { Command::new("sh") };
        c.args(if cfg!(windows) { ["/C", script] } else { ["-c", script] });
        c
    }

    /// A child that keeps running until it is killed (or half a minute passes).
    fn running() -> Command {
        // Started directly, not through a shell: killing a shell would leave its child behind.
        let mut c = if cfg!(windows) { Command::new("ping") } else { Command::new("sleep") };
        c.args(if cfg!(windows) { &["-n", "30", "127.0.0.1"][..] } else { &["30"][..] })
            .stdout(std::process::Stdio::null());
        c
    }

    fn exits(code: i32) -> Command {
        shell(&format!("exit {code}"))
    }

    /// Wait for `sup`'s child to finish; its exit code.
    fn finished(sup: &mut Supervised) -> Option<i32> {
        sup.child.as_mut().unwrap().wait().unwrap().code()
    }

    #[test]
    fn the_child_is_told_its_mode_and_why_it_was_restarted() {
        let env = |c: &Command, k: &str| {
            c.get_envs().find(|(key, _)| *key == k).map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let mut sup = Supervised::new("service");
        let c = command(std::path::Path::new("astralguard"), &sup);
        assert_eq!(c.get_args().collect::<Vec<_>>(), ["service"]);
        assert_eq!(env(&c, "GUARD_WATCHDOG"), Some(Some("1".into())));
        assert_eq!(env(&c, "GUARD_WATCHDOG_RESTART"), Some(None), "a first start: not inherited from anywhere");
        sup.restart_reason = Some("exit code 0x00000065 (panic) after 0.2 h".into());
        let c = command(std::path::Path::new("astralguard"), &sup);
        assert_eq!(env(&c, "GUARD_WATCHDOG_RESTART"), Some(Some("exit code 0x00000065 (panic) after 0.2 h".into())));
    }

    #[test]
    fn a_started_child_is_watched_until_it_exits() {
        let job = job::create();
        let mut sup = Supervised::new("ui");
        start_with(&mut sup, &job, exits(3));
        assert!(sup.child.is_some() && sup.restart_at.is_none());
        let until = Instant::now() + Duration::from_secs(10);
        let code = loop {
            if let Some(code) = poll(&mut sup) {
                break code;
            }
            assert!(Instant::now() < until, "the child never finished");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(code, Some(3));
        assert!(sup.child.is_none(), "reaped");
        assert_eq!(poll(&mut sup), None, "nothing left to poll");
    }

    #[test]
    fn a_child_that_cannot_start_is_retried_like_a_crash() {
        let job = job::create();
        let mut sup = Supervised::new("service");
        start_with(&mut sup, &job, Command::new("astralguard-no-such-program"));
        assert!(sup.child.is_none());
        assert_eq!(delay(&sup), 5);
        assert!(sup.restart_reason.as_deref().unwrap().starts_with("exit code 0xFFFFFFFF"));
    }

    #[test]
    fn due_children_are_started_and_others_wait() {
        let mut service = Supervised::new("service");
        let mut ui = Supervised::new("ui");
        ui.restart_at = Some(Instant::now() + Duration::from_secs(60));
        let started = std::cell::RefCell::new(Vec::new());
        let mut start = |sup: &mut Supervised| {
            started.borrow_mut().push(sup.mode);
            sup.restart_at = None;
            sup.child = Some(running().spawn().unwrap());
        };
        assert!(supervise(&mut service, Some(&mut ui), &mut start));
        assert_eq!(*started.borrow(), ["service"], "the UI's restart is not due yet");
        assert!(supervise(&mut service, Some(&mut ui), &mut start));
        assert_eq!(*started.borrow(), ["service"], "a running child is not started twice");
        service.child.as_mut().unwrap().kill().unwrap();
        service.child.as_mut().unwrap().wait().unwrap();
    }

    #[test]
    fn a_clean_service_exit_ends_the_watchdog_and_the_dashboard() {
        let mut service = Supervised::new("service");
        service.child = Some(exits(0).spawn().unwrap());
        finished(&mut service);
        let mut ui = Supervised::new("ui");
        ui.child = Some(running().spawn().unwrap());
        let mut no_start = |sup: &mut Supervised| panic!("{} started", sup.mode);
        assert!(!supervise(&mut service, Some(&mut ui), &mut no_start));
        let t = Instant::now();
        assert_ne!(finished(&mut ui), Some(0), "killed");
        assert!(t.elapsed() < Duration::from_secs(10), "and not left running");
    }

    #[test]
    fn a_crashed_service_is_restarted_and_the_dashboard_left_alone() {
        let mut service = Supervised::new("service");
        service.child = Some(exits(7).spawn().unwrap());
        finished(&mut service);
        let mut ui = Supervised::new("ui");
        ui.child = Some(running().spawn().unwrap());
        let mut no_start = |sup: &mut Supervised| panic!("{} started", sup.mode);
        assert!(supervise(&mut service, Some(&mut ui), &mut no_start));
        assert_eq!(delay(&service), 5);
        assert!(service.restart_reason.as_deref().unwrap().starts_with("exit code 0x00000007"));
        assert_eq!(ui.child.as_mut().unwrap().try_wait().unwrap(), None, "the dashboard keeps running");
        ui.child.as_mut().unwrap().kill().unwrap();
        ui.child.as_mut().unwrap().wait().unwrap();
    }

    #[test]
    fn a_closed_dashboard_stays_closed_and_a_crashed_one_comes_back() {
        let mut service = Supervised::new("service");
        service.restart_at = None; // not this test's business
        let mut no_start = |sup: &mut Supervised| panic!("{} started", sup.mode);

        let mut ui = Supervised::new("ui");
        ui.child = Some(exits(0).spawn().unwrap());
        finished(&mut ui);
        assert!(supervise(&mut service, Some(&mut ui), &mut no_start));
        assert!(ui.restart_at.is_none() && ui.child.is_none(), "closed by its user: stays closed");
        assert!(supervise(&mut service, Some(&mut ui), &mut no_start), "and is not started again");

        ui.child = Some(exits(101).spawn().unwrap());
        finished(&mut ui);
        assert!(supervise(&mut service, Some(&mut ui), &mut no_start));
        assert_eq!(delay(&ui), 5, "a crashed dashboard is restarted");
        assert!(supervise(&mut service, None, &mut no_start), "--no-ui: only the service");
    }
}
