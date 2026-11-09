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
        }
    }

    /// Changes the service state: poll right after, so the UI catches up at once.
    fn mutates(&self) -> bool {
        matches!(
            self,
            Job::SetPaused(_) | Job::Apply(_) | Job::ResetDefaults | Job::Calibrate | Job::ImuFast(_) | Job::ScanSensor
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
        notices.poll(&mut *backend);
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
