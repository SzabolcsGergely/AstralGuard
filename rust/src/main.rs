// No console window on Windows: every mode logs to a file or shows a window.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod config;
mod crash;
mod ipc;
mod logger;
mod mitigation;
mod platform;
mod protocol;
mod sensor;
mod service;
#[cfg(feature = "ui")]
mod ui;
mod watchdog;

/// Unix time in seconds, the unit every timer in the guard uses.
pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().cloned().unwrap_or_default();
    let rest = args.get(1..).unwrap_or(&[]);
    let code = match mode.as_str() {
        "service" => service::run(rest),
        #[cfg(feature = "ui")]
        "ui" => ui::run(rest),
        "" | "watchdog" => watchdog::run(rest),
        "probe" => probe(),
        "status" => status(rest),
        _ => {
            eprintln!("usage: astralguard [watchdog [--no-ui] | service | ui [--demo] | probe]");
            2
        }
    };
    std::process::exit(code);
}

/// `astralguard status [--exit]`: ask the running service for its state (or to stop).
fn status(args: &[String]) -> i32 {
    use protocol::{Request, Response};
    let mut client = ipc::Client::new();
    if args.iter().any(|a| a == "--exit") {
        return match client.request(&Request::Exit) {
            Ok(_) => {
                println!("exit requested");
                0
            }
            Err(e) => {
                println!("{e}");
                1
            }
        };
    }
    match client.request(&Request::Live) {
        Ok(Response::Live { live }) => {
            println!(
                "{} v{} | {:?} | {} | card: {}",
                protocol::APP_NAME,
                live.version,
                live.status,
                live.last_message,
                live.card
            );
            println!("pins: {}", live.pins.iter().map(|(n, a)| format!("{n} {a:.2}A")).collect::<Vec<_>>().join(", "));
            println!(
                "mitigation: {:?} | engaged: {} | shutdown-only: {} | sag: {:?} {:?} | imbalance: {:?}%",
                live.mitigation,
                live.engaged,
                live.effective_shutdown_only,
                live.sag_status,
                live.sag_deg,
                live.imbalance_pct
            );
            println!(
                "history samples: {} | imu samples: {} | log: {}",
                live.history.len(),
                live.imu_history.len(),
                live.log_path
            );
            0
        }
        Ok(other) => {
            println!("unexpected answer: {other:?}");
            1
        }
        Err(e) => {
            println!("{e}");
            1
        }
    }
}

/// `astralguard probe`: read the card a few times and print it. Read-only, like the guard;
/// safe next to a running guard (they only share the I2C bus for a moment).
fn probe() -> i32 {
    let mut sensor = match sensor::PinSensor::connect() {
        Ok(s) => s,
        Err(e) => {
            println!("no sensor: {e}");
            return 1;
        }
    };
    println!("{} (subsystem 0x{:08X})", sensor.description(), sensor.info.subsystem);
    for _ in 0..5 {
        match sensor.read_pins() {
            Ok(pins) => println!(
                "{}",
                pins.iter()
                    .map(|p| format!("{} {:.3}A {:.3}V", p.name, p.amps, p.volts))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
            Err(e) => println!("pins: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    for _ in 0..3 {
        match sensor.read_gsensor() {
            Ok(v) => println!("g: x {:+.4} y {:+.4} z {:+.4}", v[0], v[1], v[2]),
            Err(e) => println!("g: {e}"),
        }
    }
    println!("elevated: {}", platform::is_elevated());
    0
}
