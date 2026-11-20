//! The worker thread: the only place that talks to the service. It polls the live state
//! and runs queued commands, so the UI thread never waits on a socket.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::ipc;
use crate::protocol::{Live, Request, Response, SettingsPatch};

/// Where answers come from: the service over IPC, or the demo simulation.
pub trait Backend: Send {
    fn request(&mut self, req: &Request) -> Result<Response, String>;
}

impl Backend for ipc::Client {
    fn request(&mut self, req: &Request) -> Result<Response, String> {
        ipc::Client::request(self, req)
    }
}

/// A command from the UI. The answer comes back paired with the job that asked.
#[derive(Clone, Debug)]
pub enum Job {
    SetPaused(bool),
    Check(SettingsPatch),
    Apply(SettingsPatch),
    ResetDefaults,
    Calibrate,
    ImuFast(f64),
    Exit,
    AutostartGet,
    AutostartSet(bool),
    LogTail,
    FlightDumps,
    ReadFlight(String),
    ScanSensor,
    /// Apply now: the overclock keys saved and written to the GPU (can take ~10 s).
    ApplyOverclock(SettingsPatch),
    /// The overclock state from the driver; only sent while the Overclock tab is visible.
    OverclockStatus,
    /// The Overclock tab's live readings (about once a second while it is visible).
    OverclockLive,
}

impl Job {
    fn request(&self) -> Request {
        match self {
            Job::SetPaused(p) => Request::SetPaused { paused: *p },
            Job::Check(c) => Request::Check { changes: c.clone() },
            Job::Apply(c) => Request::Apply { changes: c.clone() },
            Job::ResetDefaults => Request::ResetDefaults,
            Job::Calibrate => Request::CalibrateSag,
            Job::ImuFast(s) => Request::ImuFast { seconds: *s },
            Job::Exit => Request::Exit,
            Job::AutostartGet => Request::Autostart,
            Job::AutostartSet(e) => Request::SetAutostart { enable: *e },
            Job::LogTail => Request::LogTail { max_bytes: 65536 },
            Job::FlightDumps => Request::FlightDumps,
            Job::ReadFlight(n) => Request::ReadFlight { name: n.clone() },
            Job::ScanSensor => Request::ScanSensor,
            Job::ApplyOverclock(c) => Request::ApplyOverclock { changes: c.clone() },
            Job::OverclockStatus => Request::OverclockStatus,
            Job::OverclockLive => Request::OverclockLive,
        }
    }

    /// Changes the service state: poll right after, so the UI catches up at once.
    fn mutates(&self) -> bool {
        matches!(
            self,
            Job::SetPaused(_)
                | Job::Apply(_)
                | Job::ResetDefaults
                | Job::Calibrate
                | Job::ImuFast(_)
                | Job::ScanSensor
                | Job::ApplyOverclock(_)
        )
    }
}

#[derive(Default)]
pub struct Shared {
    /// Last good snapshot; kept while the service is away so the tray still has data.
    pub live: Option<Arc<Live>>,
    pub connected: bool,
    /// Why the last request failed, "" when it did not.
    pub error: String,
    /// When the last snapshot arrived.
    pub live_at: Option<Instant>,
    pub done: Vec<(Job, Result<Response, String>)>,
}

/// What the UI thread and the tray read; the worker writes.
pub struct Link {
    pub shared: Mutex<Shared>,
    /// The dashboard is on screen: poll faster.
    pub window_visible: AtomicBool,
    /// The IMU view wants 10 Hz.
    pub fast: AtomicBool,
    pub quit: AtomicBool,
    /// Raise the service's notices on the desktop (Linux). Off in a dashboard window
    /// process: the tray process already does, and twice would be noise.
    pub notices: AtomicBool,
    /// The user stopped the guard from this window (Exit).
    pub guard_stopped: AtomicBool,
    repaint: Mutex<Option<egui::Context>>,
    jobs: Mutex<Option<Sender<Job>>>,
}

impl Link {
    pub fn new() -> Arc<Link> {
        Arc::new(Link {
            shared: Mutex::new(Shared::default()),
            window_visible: AtomicBool::new(true),
            fast: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            notices: AtomicBool::new(true),
            guard_stopped: AtomicBool::new(false),
            repaint: Mutex::new(None),
            jobs: Mutex::new(None),
        })
    }

    pub fn set_ctx(&self, ctx: &egui::Context) {
        *self.repaint.lock().unwrap() = Some(ctx.clone());
    }

    pub fn repaint(&self) {
        if let Some(ctx) = self.repaint.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    pub fn send(&self, job: Job) {
        if let Some(tx) = self.jobs.lock().unwrap().as_ref() {
            let _ = tx.send(job);
        }
    }

    pub fn live(&self) -> (Option<Arc<Live>>, bool) {
        let s = self.shared.lock().unwrap();
        (s.live.clone(), s.connected)
    }

    pub fn take_done(&self) -> Vec<(Job, Result<Response, String>)> {
        std::mem::take(&mut self.shared.lock().unwrap().done)
    }
}

pub fn start(link: Arc<Link>, backend: Box<dyn Backend>) -> thread::JoinHandle<()> {
    let (tx, rx) = mpsc::channel();
    *link.jobs.lock().unwrap() = Some(tx);
    thread::Builder::new().name("ui-ipc".into()).spawn(move || worker(link, backend, rx)).expect("spawn ipc worker")
}

fn normalize(r: Result<Response, String>) -> Result<Response, String> {
    match r {
        Ok(Response::Error { message }) => Err(message),
        other => other,
    }
}

fn worker(link: Arc<Link>, mut backend: Box<dyn Backend>, rx: Receiver<Job>) {
    let mut next_live = Instant::now();
    #[cfg(not(windows))]
    let mut notices = notices::Notices::default();
    while !link.quit.load(Ordering::Relaxed) {
        let wait = next_live.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(job) => {
                let result = normalize(backend.request(&job.request()));
                let poll_now = job.mutates();
                link.shared.lock().unwrap().done.push((job, result));
                link.repaint();
                if poll_now {
                    next_live = Instant::now();
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if Instant::now() < next_live {
            continue;
        }
        let result = normalize(backend.request(&Request::Live));
        let interval = {
            let mut s = link.shared.lock().unwrap();
            match result {
                Ok(Response::Live { live }) => {
                    s.live = Some(Arc::new(*live));
                    s.connected = true;
                    s.error.clear();
                    s.live_at = Some(Instant::now());
                }
                Ok(other) => {
                    s.connected = true;
                    s.error = format!("unexpected answer: {other:?}");
                }
                Err(e) => {
                    s.connected = false;
                    s.error = e;
                }
            }
            let fast = s.live.as_ref().is_some_and(|l| l.sag_fast);
            if !s.connected {
                Duration::from_millis(1000)
            } else if !link.window_visible.load(Ordering::Relaxed) {
                // only the tray is looking
                Duration::from_millis(1000)
            } else if fast && link.fast.load(Ordering::Relaxed) {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(250)
            }
        };
        next_live = Instant::now() + interval;
        link.repaint();

        #[cfg(not(windows))]
        if link.notices.load(Ordering::Relaxed) {
            notices.poll(&mut *backend);
        }
    }
}

/// Linux: a root service cannot reach the user's desktop, so the UI shows its notices.
/// (On Windows the service raises its own toasts.)
#[cfg(not(windows))]
mod notices {
    use super::Backend;
    use crate::protocol::{Request, Response};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    pub struct Notices {
        since: u64,
        next: Option<Instant>,
    }

    impl Notices {
        pub fn poll(&mut self, backend: &mut dyn Backend) {
            let now = Instant::now();
            if self.next.is_some_and(|n| now < n) {
                return;
            }
            self.next = Some(now + Duration::from_secs(2));
            let Ok(Response::Events { notices }) = backend.request(&Request::Events { since: self.since }) else {
                return;
            };
            let wall = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            for n in notices {
                self.since = self.since.max(n.seq);
                // after a UI restart, old news stays old
                if wall - n.time > 60.0 {
                    continue;
                }
                let _ = notify_rust::Notification::new()
                    .appname(crate::protocol::APP_NAME)
                    .summary(&n.title)
                    .body(&n.message)
                    .show();
            }
        }
    }
}

/// Safety rule 3 from the dashboard's side: it keeps working, and keeps retrying, while the
/// service is away, and every command's answer comes back to the job that asked.
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Service {
        up: bool,
        /// Every request, in order, by its JSON "op".
        seen: Vec<String>,
    }

    struct Fake(Arc<Mutex<Service>>);

    impl Backend for Fake {
        fn request(&mut self, req: &Request) -> Result<Response, String> {
            let mut s = self.0.lock().unwrap();
            let op = serde_json::to_value(req).unwrap()["op"].as_str().unwrap_or_default().to_string();
            s.seen.push(op);
            if !s.up {
                return Err("The service is not running".into());
            }
            Ok(match req {
                Request::Live => Response::Live { live: Box::new(Live { max_val: 7.5, ..Live::default() }) },
                Request::CalibrateSag => Response::Error { message: "no IMU".into() },
                _ => Response::Ok,
            })
        }
    }

    fn start_fake(up: bool) -> (Arc<Link>, Arc<Mutex<Service>>, thread::JoinHandle<()>) {
        let service = Arc::new(Mutex::new(Service { up, ..Service::default() }));
        let link = Link::new();
        // Polls once a second: a poll right after a command is then easy to tell apart.
        link.window_visible.store(false, Ordering::Relaxed);
        link.notices.store(false, Ordering::Relaxed);
        let worker = start(link.clone(), Box::new(Fake(service.clone())));
        (link, service, worker)
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop(link: &Link, worker: thread::JoinHandle<()>) {
        link.quit.store(true, Ordering::Relaxed);
        link.send(Job::LogTail); // wake it
        worker.join().unwrap();
    }

    #[test]
    fn the_dashboard_keeps_its_last_snapshot_and_retries_while_the_service_is_away() {
        let (link, service, worker) = start_fake(true);
        wait_for("the first snapshot", || link.live().1);
        assert_eq!(link.live().0.unwrap().max_val, 7.5);

        service.lock().unwrap().up = false;
        wait_for("the outage to show", || !link.live().1);
        let (live, connected) = link.live();
        assert!(!connected);
        assert_eq!(live.unwrap().max_val, 7.5, "the tray keeps the last reading to show");
        assert_eq!(link.shared.lock().unwrap().error, "The service is not running");

        let polls = service.lock().unwrap().seen.len();
        wait_for("a retry", || service.lock().unwrap().seen.len() > polls);
        service.lock().unwrap().up = true;
        wait_for("the reconnect", || link.live().1);
        assert!(link.shared.lock().unwrap().error.is_empty());
        stop(&link, worker);
    }

    #[test]
    fn every_answer_comes_back_to_the_job_that_asked() {
        let (link, _service, worker) = start_fake(true);
        link.send(Job::ScanSensor);
        link.send(Job::Calibrate);
        wait_for("both answers", || link.shared.lock().unwrap().done.len() == 2);
        let done = link.take_done();
        assert!(matches!(&done[0], (Job::ScanSensor, Ok(Response::Ok))));
        // The service's Error answer reaches the dashboard as a failed job.
        assert!(matches!(&done[1], (Job::Calibrate, Err(m)) if m == "no IMU"));
        assert!(link.take_done().is_empty(), "taken once");
        stop(&link, worker);
    }

    #[test]
    fn a_command_that_changes_the_service_is_followed_by_a_poll() {
        let (link, service, worker) = start_fake(true);
        wait_for("the first snapshot", || link.live().1);
        let ops_after = |n: usize| service.lock().unwrap().seen[n..].to_vec();

        let n = service.lock().unwrap().seen.len();
        link.send(Job::SetPaused(true));
        thread::sleep(Duration::from_millis(400)); // well inside the 1 s poll
        assert_eq!(ops_after(n), ["set_paused", "live"], "the pause shows at once");

        let n = service.lock().unwrap().seen.len();
        link.send(Job::LogTail);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(ops_after(n), ["log_tail"], "reading the log changes nothing");
        stop(&link, worker);
    }

    #[test]
    fn jobs_ask_for_the_right_thing() {
        let patch = SettingsPatch::default();
        let ops: Vec<(Job, &str)> = vec![
            (Job::SetPaused(true), "set_paused"),
            (Job::Check(patch.clone()), "check"),
            (Job::Apply(patch.clone()), "apply"),
            (Job::ResetDefaults, "reset_defaults"),
            (Job::Calibrate, "calibrate_sag"),
            (Job::ImuFast(30.0), "imu_fast"),
            (Job::Exit, "exit"),
            (Job::AutostartGet, "autostart"),
            (Job::AutostartSet(true), "set_autostart"),
            (Job::LogTail, "log_tail"),
            (Job::FlightDumps, "flight_dumps"),
            (Job::ReadFlight("x.csv".into()), "read_flight"),
            (Job::ScanSensor, "scan_sensor"),
            (Job::ApplyOverclock(patch), "apply_overclock"),
            (Job::OverclockStatus, "overclock_status"),
            (Job::OverclockLive, "overclock_live"),
        ];
        for (job, op) in &ops {
            assert_eq!(serde_json::to_value(job.request()).unwrap()["op"], *op, "{job:?}");
        }
        let mutating: Vec<_> = ops.iter().filter(|(j, _)| j.mutates()).map(|(_, op)| *op).collect();
        assert_eq!(
            mutating,
            ["set_paused", "apply", "reset_defaults", "calibrate_sag", "imu_fast", "scan_sensor", "apply_overclock"]
        );
        let Request::LogTail { max_bytes } = Job::LogTail.request() else { panic!() };
        assert_eq!(max_bytes, 65536);
    }
}
