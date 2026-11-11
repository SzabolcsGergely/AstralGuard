//! Local socket between the service and the UI: newline-delimited JSON, one request, one
//! response. A named pipe on Windows (writable only by the elevated owner and admins, so an
//! ordinary process cannot pause the guard), a socket file on Linux (root and the
//! `astralguard` group).

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::thread;

use interprocess::local_socket::{prelude::*, ListenerOptions, Name, Stream};

use crate::protocol::{socket_name, Request, Response};

fn name(socket: &str) -> std::io::Result<Name<'_>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        socket.to_ns_name::<GenericNamespaced>()
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket.to_fs_name::<GenericFilePath>()
    }
}

/// Serve requests until the process exits. Each connection gets its own thread; the
/// handler must be quick and must never block on the UI.
pub fn serve<F>(handler: F) -> std::io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    serve_at(socket_name(), handler)
}

/// `serve` on another socket: a pipe name on Windows, a file path on Linux (tests).
fn serve_at<F>(socket: &str, handler: F) -> std::io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    #[cfg(unix)]
    prepare_socket_dir(socket);
    let listener = ListenerOptions::new().name(name(socket)?).create_sync()?;
    #[cfg(unix)]
    restrict_socket(socket);
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
fn prepare_socket_dir(socket: &str) {
    let path = std::path::Path::new(socket);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    }
    // A socket file left by a previous run would make the bind fail.
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn restrict_socket(path: &str) {
    use std::ffi::CString;
    use std::os::unix::fs::PermissionsExt;
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
pub struct Client {
    conn: Option<BufReader<Stream>>,
    socket: String,
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

#[cfg_attr(not(feature = "ui"), allow(dead_code))]
impl Client {
    pub fn new() -> Self {
        Client::at(socket_name())
    }

    fn at(socket: &str) -> Self {
        Client { conn: None, socket: socket.into() }
    }

    pub fn request(&mut self, req: &Request) -> Result<Response, String> {
        if self.conn.is_none() {
            let stream = Stream::connect(name(&self.socket).map_err(|e| e.to_string())?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A socket of the test's own: a pipe name on Windows, a file in a fresh folder on Linux.
    fn test_socket() -> String {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let id = format!("ag-ipc-{}-{n}", std::process::id());
        if cfg!(windows) {
            format!("{id}.sock")
        } else {
            let dir = std::env::temp_dir().join(id);
            let _ = std::fs::remove_dir_all(&dir);
            dir.join("astralguard.sock").to_string_lossy().into_owned()
        }
    }

    /// Answers LogTail with its size, so a test can tell the replies apart.
    fn echo(req: Request) -> Response {
        match req {
            Request::LogTail { max_bytes } => Response::Text { text: max_bytes.to_string() },
            _ => Response::Ok,
        }
    }

    fn text(r: Result<Response, String>) -> String {
        match r {
            Ok(Response::Text { text }) => text,
            other => panic!("not a text answer: {other:?}"),
        }
    }

    #[test]
    fn a_request_gets_its_own_answer_on_one_connection() {
        let socket = test_socket();
        serve_at(&socket, echo).unwrap();
        let mut client = Client::at(&socket);
        assert_eq!(text(client.request(&Request::LogTail { max_bytes: 7 })), "7");
        assert_eq!(text(client.request(&Request::LogTail { max_bytes: 42 })), "42");
        assert!(client.conn.is_some(), "the connection is kept between requests");
        let mut other = Client::at(&socket);
        assert_eq!(text(other.request(&Request::LogTail { max_bytes: 3 })), "3", "and served alongside another");
    }

    #[test]
    fn a_malformed_request_gets_an_error_and_the_connection_stays_up() {
        let socket = test_socket();
        serve_at(&socket, echo).unwrap();
        let mut conn = BufReader::new(Stream::connect(name(&socket).unwrap()).unwrap());
        conn.get_mut().write_all(b"{\"kind\":\"no_such_request\"}\n").unwrap();
        let mut answer = String::new();
        conn.read_line(&mut answer).unwrap();
        let Response::Error { message } = serde_json::from_str(answer.trim()).unwrap() else { panic!("{answer}") };
        assert!(message.starts_with("bad request"), "{message}");
        let req = serde_json::to_string(&Request::LogTail { max_bytes: 5 }).unwrap() + "\n";
        conn.get_mut().write_all(req.as_bytes()).unwrap();
        answer.clear();
        conn.read_line(&mut answer).unwrap();
        assert!(matches!(serde_json::from_str(answer.trim()).unwrap(), Response::Text { text } if text == "5"));
    }

    #[test]
    fn without_a_service_the_client_says_so_and_connects_once_there_is_one() {
        let socket = test_socket();
        let mut client = Client::at(&socket);
        let e = client.request(&Request::Live).unwrap_err();
        assert!(e.contains("not running"), "{e}");
        serve_at(&socket, echo).unwrap();
        assert_eq!(text(client.request(&Request::LogTail { max_bytes: 1 })), "1", "picked up without a restart");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_is_for_root_and_the_group_only_and_replaces_a_stale_one() {
        use std::os::unix::fs::PermissionsExt;
        let socket = test_socket();
        std::fs::create_dir_all(std::path::Path::new(&socket).parent().unwrap()).unwrap();
        std::fs::write(&socket, "left by a crashed run").unwrap();
        serve_at(&socket, echo).unwrap();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "no access for other users: {mode:o}");
        assert_eq!(text(Client::at(&socket).request(&Request::LogTail { max_bytes: 2 })), "2");
        let _ = std::fs::remove_dir_all(std::path::Path::new(&socket).parent().unwrap());
    }
}
