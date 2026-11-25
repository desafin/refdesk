//! Single-instance support: a second `refdesk` process hands its request to the running one over
//! a Unix socket (`docs/ARCHITECTURE.md` §4.11).
//!
//! One request is one line of JSON; the answer is one line, `ok` or `error: <reason>`. This
//! module never reads the environment: the socket path is an argument (see
//! [`crate::store::Paths::socket_path`]).
//!
//! Paths in a request are made absolute by the *sender* (see [`IpcRequest::with_absolute_paths`]),
//! because the receiving process has a different current directory.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a read or write on a connection may take, on both sides.
const IO_TIMEOUT: Duration = Duration::from_secs(1);

/// The longest request line the listener accepts, in bytes.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// The longest answer line the sender reads, in bytes.
const MAX_RESPONSE_BYTES: u64 = 4096;

/// The marker that starts a page request in a command-line argument.
const PAGE_MARKER: &str = "#page=";

/// A file to open, optionally at a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTarget {
    /// The file. Absolute when it travels between processes; the app also accepts a relative path
    /// and normalizes it when opening.
    pub path: PathBuf,
    /// Zero-based page to show first.
    pub page: Option<u32>,
}

impl OpenTarget {
    /// Parses a command-line argument: `a.pdf` or `a.pdf#page=12`.
    ///
    /// The page number is 1-based for people and becomes 0-based here. A fragment that is not a
    /// positive number is dropped (and logged) so the file still opens, at its first page.
    /// Only the exact marker `#page=` counts, so a file name that contains `#` is taken whole.
    pub fn parse(arg: &str) -> OpenTarget {
        if let Some(at) = arg.rfind(PAGE_MARKER) {
            let (path, rest) = (&arg[..at], &arg[at + PAGE_MARKER.len()..]);
            if !path.is_empty() {
                let page = match rest.parse::<u32>() {
                    Ok(n) if n >= 1 => Some(n - 1),
                    _ => {
                        log::warn!("ignoring invalid page number in argument {arg:?}");
                        None
                    }
                };
                return OpenTarget {
                    path: PathBuf::from(path),
                    page,
                };
            }
        }
        OpenTarget {
            path: PathBuf::from(arg),
            page: None,
        }
    }
}

/// What one process asks another to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Open these files. An empty list only asks the window to come forward.
    Open {
        /// The files, in the order they were given.
        targets: Vec<OpenTarget>,
    },
    /// Compare two documents.
    Diff {
        /// The older document.
        left: PathBuf,
        /// The newer document.
        right: PathBuf,
    },
}

/// Makes `path` absolute by putting it under `base`. An absolute path is returned unchanged.
/// Nothing is resolved or cleaned: `..` and symlinks stay as written.
pub fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

impl IpcRequest {
    /// The same request with every relative path put under `base` (the sender's current
    /// directory). Pages and the order of files are kept.
    pub fn with_absolute_paths(self, base: &Path) -> IpcRequest {
        match self {
            IpcRequest::Open { targets } => IpcRequest::Open {
                targets: targets
                    .into_iter()
                    .map(|t| OpenTarget {
                        path: absolute_path(&t.path, base),
                        page: t.page,
                    })
                    .collect(),
            },
            IpcRequest::Diff { left, right } => IpcRequest::Diff {
                left: absolute_path(&left, base),
                right: absolute_path(&right, base),
            },
        }
    }
}

fn invalid_data(err: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

/// Sends `req` to the instance listening on `socket`.
///
/// * `Ok(true)`: an instance took the request and answered `ok`.
/// * `Ok(false)`: nobody is there to take it — the socket file is missing, the connection is
///   refused (a stale file), the peer does not answer within one second, or it answered something
///   else. The caller should start a new instance.
/// * `Err`: the request could not be encoded (a path that is not UTF-8) or the socket cannot be
///   used for another reason (for example, permission denied).
pub fn send(socket: &Path, req: &IpcRequest) -> io::Result<bool> {
    let mut line = serde_json::to_string(req).map_err(invalid_data)?;
    line.push('\n');

    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(err) => return Err(err),
    };
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    // From here on a failure means the peer is not a healthy refdesk: treat it as absent.
    let answered = (|| -> io::Result<String> {
        (&stream).write_all(line.as_bytes())?;
        let mut response = String::new();
        BufReader::new((&stream).take(MAX_RESPONSE_BYTES)).read_line(&mut response)?;
        Ok(response)
    })();
    match answered {
        Ok(response) => Ok(response.trim_end() == "ok"),
        Err(err) => {
            log::debug!("no usable answer from the running instance: {err}");
            Ok(false)
        }
    }
}

/// Keeps the listening thread and the socket file alive. Dropping it stops the thread and
/// removes the socket file.
pub struct IpcListener {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // `accept` blocks, so wake the thread with a connection of our own. It sees the flag
        // before it reads anything. If the wake-up cannot connect (the file was removed behind
        // our back) the thread cannot be woken: leave it and do not wait.
        let woken = UnixStream::connect(&self.socket).is_ok();
        if let Some(thread) = self.thread.take()
            && woken
            && thread.join().is_err()
        {
            log::error!("the ipc listener thread panicked");
        }
        if let Err(err) = std::fs::remove_file(&self.socket)
            && err.kind() != io::ErrorKind::NotFound
        {
            log::warn!("could not remove {}: {err}", self.socket.display());
        }
    }
}

/// Starts listening on `socket` and calls `on_request` (on the listener's thread) for every
/// well-formed request.
///
/// A socket file that already exists is probed first: if something accepts the connection,
/// another instance is alive and this fails with [`io::ErrorKind::AddrInUse`]; if the connection
/// is refused the file is a leftover and is replaced. Something at that path that is not a
/// socket is never touched ([`io::ErrorKind::AlreadyExists`]).
///
/// The socket file is readable and writable by its owner only (0600).
pub fn listen(
    socket: &Path,
    on_request: impl Fn(IpcRequest) + Send + 'static,
) -> io::Result<IpcListener> {
    match std::fs::symlink_metadata(socket) {
        Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(socket) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another instance is listening on {}", socket.display()),
                ));
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                std::fs::remove_file(socket)?;
            }
            Err(err) => return Err(err),
        },
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", socket.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = socket.parent() {
                std::fs::create_dir_all(dir)?;
            }
        }
        Err(err) => return Err(err),
    }

    let listener = UnixListener::bind(socket)?;
    if let Err(err) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(socket);
        return Err(err);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("ipc-listener".into())
        .spawn(move || accept_loop(listener, thread_stop, on_request));
    match spawned {
        Ok(thread) => Ok(IpcListener {
            socket: socket.to_path_buf(),
            stop,
            thread: Some(thread),
        }),
        Err(err) => {
            let _ = std::fs::remove_file(socket);
            Err(err)
        }
    }
}

fn accept_loop(listener: UnixListener, stop: Arc<AtomicBool>, on_request: impl Fn(IpcRequest)) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match stream {
            Ok(stream) => handle_connection(&stream, &on_request),
            Err(err) => {
                log::warn!("ipc accept failed: {err}");
                // Do not spin if accept keeps failing (for example, out of file descriptors).
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Reads one request line from `stream`, passes it on, and answers. Problems with this one
/// connection are answered or logged, never propagated.
fn handle_connection(stream: &UnixStream, on_request: &impl Fn(IpcRequest)) {
    if stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return;
    }
    let mut line = Vec::new();
    // One byte more than the limit tells "exactly at the limit" from "too long".
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES + 1));
    match reader.read_until(b'\n', &mut line) {
        // The peer connected and left without a word (a liveness probe).
        Ok(0) => return,
        Ok(_) => {}
        Err(err) => {
            log::debug!("ipc: could not read a request: {err}");
            return;
        }
    }

    let reply = if line.len() as u64 > MAX_REQUEST_BYTES {
        "error: request too long".to_owned()
    } else if line.last() != Some(&b'\n') {
        "error: request is not terminated by a newline".to_owned()
    } else {
        match serde_json::from_slice::<IpcRequest>(&line) {
            Ok(request) => {
                if catch_unwind(AssertUnwindSafe(|| on_request(request))).is_err() {
                    log::error!("ipc: the request handler panicked");
                    "error: internal error".to_owned()
                } else {
                    "ok".to_owned()
                }
            }
            Err(err) => format!("error: {err}"),
        }
    };
    // The answer is one line, whatever the parser's message looks like.
    let reply = reply.replace(['\n', '\r'], " ");
    if let Err(err) = (&*stream).write_all(format!("{reply}\n").as_bytes()) {
        log::debug!("ipc: could not answer: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(5);

    fn open(path: &str, page: Option<u32>) -> IpcRequest {
        IpcRequest::Open {
            targets: vec![OpenTarget {
                path: PathBuf::from(path),
                page,
            }],
        }
    }

    /// A listener whose requests arrive on the returned channel.
    fn collecting(socket: &Path) -> (IpcListener, Receiver<IpcRequest>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let listener = listen(socket, move |req| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(req);
            }
        })
        .expect("listen");
        (listener, rx)
    }

    /// Writes raw bytes to the socket and returns the answer line.
    fn raw_exchange(socket: &Path, bytes: &[u8]) -> String {
        let stream = UnixStream::connect(socket).expect("connect");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        (&stream).write_all(bytes).expect("write");
        let mut answer = String::new();
        BufReader::new(&stream)
            .read_line(&mut answer)
            .expect("read");
        answer
    }

    #[test]
    fn a_request_round_trips_and_is_acknowledged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);

        let request = open("/docs/a.pdf", Some(4));
        assert!(send(&socket, &request).expect("send"));
        assert_eq!(rx.recv_timeout(WAIT).expect("received"), request);

        let diff = IpcRequest::Diff {
            left: PathBuf::from("/docs/old.pdf"),
            right: PathBuf::from("/docs/new.pdf"),
        };
        assert!(send(&socket, &diff).expect("send"));
        assert_eq!(rx.recv_timeout(WAIT).expect("received"), diff);
    }

    #[test]
    fn an_empty_open_request_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);
        let request = IpcRequest::Open { targets: vec![] };
        assert!(send(&socket, &request).expect("send"));
        assert_eq!(rx.recv_timeout(WAIT).expect("received"), request);
    }

    #[test]
    fn the_wire_format_is_one_json_line() {
        let json = serde_json::to_string(&open("/a.pdf", Some(1))).expect("encode");
        assert!(!json.contains('\n'));
        let back: IpcRequest = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, open("/a.pdf", Some(1)));
    }

    #[test]
    fn send_without_a_listener_is_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("none.sock");
        assert!(!send(&missing, &open("/a.pdf", None)).expect("send"));
    }

    #[test]
    fn send_to_a_stale_socket_file_is_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("stale.sock");
        drop(UnixListener::bind(&socket).expect("bind"));
        assert!(socket.exists(), "std leaves the file behind");
        assert!(!send(&socket, &open("/a.pdf", None)).expect("send"));
    }

    #[test]
    fn listen_replaces_a_stale_socket_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("stale.sock");
        drop(UnixListener::bind(&socket).expect("bind"));

        let (_listener, rx) = collecting(&socket);
        assert!(send(&socket, &open("/a.pdf", None)).expect("send"));
        assert!(rx.recv_timeout(WAIT).is_ok());
    }

    #[test]
    fn listen_creates_the_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("deeper/still/r.sock");
        let (_listener, _rx) = collecting(&socket);
        assert!(socket.exists());
    }

    #[test]
    fn a_second_listen_fails_while_the_first_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);

        let err = listen(&socket, |_| {})
            .err()
            .expect("second listen must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

        // The first instance still works, and the failed attempt left no trace in its input.
        assert!(send(&socket, &open("/a.pdf", None)).expect("send"));
        assert_eq!(
            rx.recv_timeout(WAIT).expect("received"),
            open("/a.pdf", None)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn listen_refuses_to_replace_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("precious");
        std::fs::write(&path, b"data").expect("write");
        let err = listen(&path, |_| {}).err().expect("must fail");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).expect("read"), b"data");
    }

    #[test]
    fn the_socket_file_is_private_to_the_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, _rx) = collecting(&socket);
        let mode = std::fs::metadata(&socket)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_bad_line_gets_an_error_and_later_requests_still_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);

        let answer = raw_exchange(&socket, b"this is not json\n");
        assert!(answer.starts_with("error: "), "{answer:?}");
        let answer = raw_exchange(&socket, b"{\"type\":\"unknown\"}\n");
        assert!(answer.starts_with("error: "), "{answer:?}");
        let answer = raw_exchange(&socket, b"\n");
        assert!(answer.starts_with("error: "), "{answer:?}");
        // A request that ends without its newline is not taken either.
        let stream = UnixStream::connect(&socket).expect("connect");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        (&stream)
            .write_all(b"{\"type\":\"open\",\"targets\":[]}")
            .expect("write");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");
        let mut answer = String::new();
        BufReader::new(&stream)
            .read_line(&mut answer)
            .expect("read");
        assert!(answer.starts_with("error: "), "{answer:?}");
        assert!(rx.try_recv().is_err(), "no bad request reached the handler");

        assert!(send(&socket, &open("/a.pdf", None)).expect("send"));
        assert_eq!(
            rx.recv_timeout(WAIT).expect("received"),
            open("/a.pdf", None)
        );
    }

    #[test]
    fn a_too_long_line_is_rejected_and_the_listener_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);

        let stream = UnixStream::connect(&socket).expect("connect");
        stream.set_read_timeout(Some(WAIT)).expect("timeout");
        stream.set_write_timeout(Some(WAIT)).expect("timeout");
        let junk = vec![b'x'; MAX_REQUEST_BYTES as usize + 10];
        // The server stops reading at the limit and may close early, so a write error is fine.
        let _ = (&stream).write_all(&junk);
        let mut answer = String::new();
        let _ = BufReader::new(&stream).read_line(&mut answer);
        assert!(
            answer.is_empty() || answer.starts_with("error: "),
            "{answer:?}"
        );

        assert!(send(&socket, &open("/a.pdf", None)).expect("send"));
        assert_eq!(
            rx.recv_timeout(WAIT).expect("received"),
            open("/a.pdf", None)
        );
    }

    #[test]
    fn a_silent_connection_is_dropped_after_the_timeout_and_the_listener_recovers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);

        // Connects and says nothing. The single listener thread waits for it for one timeout
        // (a sender that arrives meanwhile may give up with `Ok(false)`), then moves on.
        let silent = UnixStream::connect(&socket).expect("connect");
        std::thread::sleep(IO_TIMEOUT + Duration::from_millis(500));
        assert!(send(&socket, &open("/a.pdf", None)).expect("send"));
        assert_eq!(
            rx.recv_timeout(WAIT).expect("received"),
            open("/a.pdf", None)
        );
        drop(silent);
    }

    #[test]
    fn send_to_a_server_that_never_answers_gives_up_with_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("mute.sock");
        // Bound but never accepting: the kernel completes the connection, nobody answers.
        let _mute = UnixListener::bind(&socket).expect("bind");

        let started = Instant::now();
        assert!(!send(&socket, &open("/a.pdf", None)).expect("send"));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(800), "{took:?}");
        assert!(took < Duration::from_secs(4), "{took:?}");
    }

    #[test]
    fn send_treats_a_foreign_answer_as_not_received() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("odd.sock");
        let server = UnixListener::bind(&socket).expect("bind");
        let thread = std::thread::spawn(move || {
            if let Ok((stream, _)) = server.accept() {
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let _ = (&stream).write_all(b"error: go away\n");
            }
        });
        assert!(!send(&socket, &open("/a.pdf", None)).expect("send"));
        thread.join().expect("server thread");
    }

    #[test]
    fn dropping_the_listener_removes_the_socket_and_ends_the_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        // The thread owns a clone of `witness`; once it ends the clone is gone.
        let witness = Arc::new(());
        let held = Arc::clone(&witness);
        let listener = listen(&socket, move |_| {
            let _ = &held;
        })
        .expect("listen");
        assert!(socket.exists());
        assert_eq!(Arc::strong_count(&witness), 2);

        drop(listener);
        assert!(!socket.exists());
        assert_eq!(Arc::strong_count(&witness), 1, "the thread is gone");
        assert!(!send(&socket, &open("/a.pdf", None)).expect("send"));

        // The path is free again.
        let (_again, _rx) = collecting(&socket);
    }

    #[test]
    fn requests_arrive_in_the_order_they_were_sent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let (_listener, rx) = collecting(&socket);

        for i in 0..50u32 {
            assert!(send(&socket, &open("/a.pdf", Some(i))).expect("send"));
        }
        for i in 0..50u32 {
            assert_eq!(
                rx.recv_timeout(WAIT).expect("received"),
                open("/a.pdf", Some(i))
            );
        }
    }

    #[test]
    fn a_panicking_handler_does_not_kill_the_listener() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("r.sock");
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        let _listener = listen(&socket, move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("first request fails");
            }
        })
        .expect("listen");

        // The first request is answered with an error, the second one works.
        assert!(!send(&socket, &open("/a.pdf", None)).expect("send"));
        assert!(send(&socket, &open("/b.pdf", None)).expect("send"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn relative_paths_become_absolute_under_the_base() {
        let base = Path::new("/work/dir");
        let request = IpcRequest::Open {
            targets: vec![
                OpenTarget {
                    path: PathBuf::from("a.pdf"),
                    page: None,
                },
                OpenTarget {
                    path: PathBuf::from("sub/../b.pdf"),
                    page: Some(2),
                },
            ],
        }
        .with_absolute_paths(base);
        assert_eq!(
            request,
            IpcRequest::Open {
                targets: vec![
                    OpenTarget {
                        path: PathBuf::from("/work/dir/a.pdf"),
                        page: None
                    },
                    OpenTarget {
                        path: PathBuf::from("/work/dir/sub/../b.pdf"),
                        page: Some(2)
                    },
                ]
            }
        );
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        let base = Path::new("/work/dir");
        assert_eq!(
            absolute_path(Path::new("/elsewhere/a.pdf"), base),
            PathBuf::from("/elsewhere/a.pdf")
        );
        let diff = IpcRequest::Diff {
            left: PathBuf::from("/x/old.pdf"),
            right: PathBuf::from("new.pdf"),
        }
        .with_absolute_paths(base);
        assert_eq!(
            diff,
            IpcRequest::Diff {
                left: PathBuf::from("/x/old.pdf"),
                right: PathBuf::from("/work/dir/new.pdf"),
            }
        );
    }

    #[test]
    fn a_page_fragment_survives_making_the_path_absolute() {
        let target = OpenTarget::parse("manual.pdf#page=12");
        let request = IpcRequest::Open {
            targets: vec![target],
        }
        .with_absolute_paths(Path::new("/work"));
        assert_eq!(
            request,
            IpcRequest::Open {
                targets: vec![OpenTarget {
                    path: PathBuf::from("/work/manual.pdf"),
                    page: Some(11)
                }]
            }
        );
    }

    #[test]
    fn parse_plain_path() {
        let t = OpenTarget::parse("docs/a.pdf");
        assert_eq!(t.path, PathBuf::from("docs/a.pdf"));
        assert_eq!(t.page, None);
    }

    #[test]
    fn parse_page_fragment_is_one_based_in_and_zero_based_out() {
        let t = OpenTarget::parse("a.pdf#page=12");
        assert_eq!(t.path, PathBuf::from("a.pdf"));
        assert_eq!(t.page, Some(11));
        assert_eq!(OpenTarget::parse("a.pdf#page=1").page, Some(0));
    }

    #[test]
    fn parse_invalid_page_values_open_the_file_at_the_start() {
        for arg in [
            "a.pdf#page=abc",
            "a.pdf#page=",
            "a.pdf#page=0",
            "a.pdf#page=-3",
            "a.pdf#page=99999999999999",
        ] {
            let t = OpenTarget::parse(arg);
            assert_eq!(t.path, PathBuf::from("a.pdf"), "{arg}");
            assert_eq!(t.page, None, "{arg}");
        }
    }

    #[test]
    fn parse_keeps_a_hash_inside_the_file_name() {
        let t = OpenTarget::parse("C#notes.pdf");
        assert_eq!(t.path, PathBuf::from("C#notes.pdf"));
        assert_eq!(t.page, None);

        let t = OpenTarget::parse("C#notes.pdf#page=7");
        assert_eq!(t.path, PathBuf::from("C#notes.pdf"));
        assert_eq!(t.page, Some(6));

        // Not the exact marker: part of the name.
        let t = OpenTarget::parse("a.pdf#pages=7");
        assert_eq!(t.path, PathBuf::from("a.pdf#pages=7"));
        assert_eq!(t.page, None);

        // No file name before the marker: take the whole argument as the path.
        let t = OpenTarget::parse("#page=7");
        assert_eq!(t.path, PathBuf::from("#page=7"));
        assert_eq!(t.page, None);
    }
}
