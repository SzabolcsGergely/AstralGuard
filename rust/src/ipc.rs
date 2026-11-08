//! Local socket between the service and the UI: newline-delimited JSON, one request, one
//! response. A named pipe on Windows (writable only by the elevated owner and admins, so an
//! ordinary process cannot pause the guard), a socket file on Linux (root and the
//! `astralguard` group).

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::thread;

use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};

use crate::protocol::{socket_name, Request, Response};

fn name() -> std::io::Result<Name<'static>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        socket_name().to_ns_name::<GenericNamespaced>()
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_name().to_fs_name::<GenericFilePath>()
    }
}

/// Serve requests until the process exits. Each connection gets its own thread; the
/// handler must be quick and must never block on the UI.
pub fn serve<F>(handler: F) -> std::io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    #[cfg(unix)]
    prepare_socket_dir();
    let listener = ListenerOptions::new().name(name()?).create_sync()?;
    #[cfg(unix)]
    restrict_socket();
    let handler = Arc::new(handler);
    thread::Builder::new().name("ipc-accept".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let handler = Arc::clone(&handler);
            let _ = thread::Builder::new().name("ipc-conn".into()).spawn(move || {
                let mut conn = BufReader::new(conn);
                let mut line = String::new();
                loop {
                    line.clear();
                    match conn.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let response = match serde_json::from_str::<Request>(line.trim()) {
                        Ok(req) => handler(req),
                        Err(e) => Response::Error { message: format!("bad request: {e}") },
                    };
                    let mut out = match serde_json::to_string(&response) {
                        Ok(s) => s,
                        Err(e) => format!(r#"{{"kind":"error","message":"{e}"}}"#),
                    };
                    out.push('\n');
                    if conn.get_mut().write_all(out.as_bytes()).is_err() {
                        break;
                    }
                }
            });
        }
    })?;
    Ok(())
}

#[cfg(unix)]
fn prepare_socket_dir() {
    let path = std::path::Path::new(socket_name());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    }
    // A socket file left by a previous run would make the bind fail.
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn restrict_socket() {
    use std::ffi::CString;
    use std::os::unix::fs::PermissionsExt;
    let path = socket_name();
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
    // Members of the "astralguard" group may run the UI; everyone else is locked out.
    if let (Ok(cpath), Ok(group)) = (CString::new(path), CString::new("astralguard")) {
        unsafe {
            let gr = libc::getgrnam(group.as_ptr());
            if !gr.is_null() {
                libc::chown(cpath.as_ptr(), u32::MAX, (*gr).gr_gid);
            }
        }
    }
}

/// The UI's side. Connects lazily and reconnects after any failure, so a restarted
/// service is picked up without restarting the UI.
#[cfg_attr(not(feature = "ui"), allow(dead_code))]
#[derive(Default)]
pub struct Client {
    conn: Option<BufReader<Stream>>,
}

#[cfg_attr(not(feature = "ui"), allow(dead_code))]
impl Client {
    pub fn new() -> Self {
        Client { conn: None }
    }

    pub fn request(&mut self, req: &Request) -> Result<Response, String> {
        if self.conn.is_none() {
            let stream = Stream::connect(name().map_err(|e| e.to_string())?)
                .map_err(|e| format!("the guard service is not running ({e})"))?;
            self.conn = Some(BufReader::new(stream));
        }
        let result = self.exchange(req);
        if result.is_err() {
            self.conn = None;
        }
        result
    }

    fn exchange(&mut self, req: &Request) -> Result<Response, String> {
        let conn = self.conn.as_mut().ok_or("not connected")?;
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        conn.get_mut().write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut answer = String::new();
        match conn.read_line(&mut answer) {
            Ok(0) => Err("the guard service closed the connection".into()),
            Ok(_) => serde_json::from_str(answer.trim()).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}
