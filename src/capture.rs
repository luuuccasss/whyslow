//! Captures the command's output: it is shown live, unchanged, and every line
//! is also kept with a timestamp. When whyslow writes to a terminal the command
//! gets a pseudo-terminal, so tools keep their colors and progress bars.

use crate::model::OutputLine;
use crate::util::{strip_ansi, truncate};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_LINES: usize = 100_000;
const MAX_LINE_CHARS: usize = 400;

/// The write ends handed to the command.
pub struct ChildIo {
    out: OwnedFd,
    err: OwnedFd,
}

impl ChildIo {
    pub fn stdio(&self) -> io::Result<(Stdio, Stdio)> {
        Ok((Stdio::from(self.out.try_clone()?), Stdio::from(self.err.try_clone()?)))
    }
}

#[derive(Default)]
struct Store {
    lines: Vec<OutputLine>,
    dropped: u64,
}

pub struct Capture {
    store: Arc<Mutex<Store>>,
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum Sink {
    Stdout,
    Stderr,
    Discard,
}

pub fn start(t0: Instant, quiet: bool) -> io::Result<(ChildIo, Capture)> {
    let store = Arc::new(Mutex::new(Store::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut spawn = |fd: OwnedFd, sink: Sink, err: bool| {
        let (store, stop) = (store.clone(), stop.clone());
        let sink = if quiet { Sink::Discard } else { sink };
        handles.push(std::thread::spawn(move || reader(fd, sink, err, t0, store, stop)));
    };

    let io = if crate::util::isatty(1) {
        let (master, slave) = open_pty()?;
        spawn(master, Sink::Stdout, false);
        ChildIo { out: slave.try_clone()?, err: slave }
    } else {
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        spawn(out_r, Sink::Stdout, false);
        spawn(err_r, Sink::Stderr, true);
        ChildIo { out: out_w, err: err_w }
    };
    Ok((io, Capture { store, stop, handles }))
}

impl Capture {
    /// Drain what is left (bounded, in case a daemon keeps the terminal open)
    /// and return the captured lines.
    pub fn finish(self) -> (Vec<OutputLine>, u64) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.handles {
            let _ = h.join();
        }
        let mut store = self.store.lock().unwrap();
        let mut lines = std::mem::take(&mut store.lines);
        lines.sort_by(|a, b| a.t_ms.total_cmp(&b.t_ms));
        (lines, store.dropped)
    }
}

fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        let have_size = libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0;
        let (mut m, mut s) = (-1, -1);
        let winp: *mut libc::winsize = if have_size { &mut ws } else { std::ptr::null_mut() };
        if libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut::<libc::termios>() as _, winp as _) != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in [m, s] {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok((OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)))
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

fn reader(fd: OwnedFd, sink: Sink, err: bool, t0: Instant, store: Arc<Mutex<Store>>, stop: Arc<AtomicBool>) {
    let raw = fd.as_raw_fd();
    let mut split = Splitter { buf: Vec::new(), start: None, err };
    let mut buf = vec![0u8; 64 * 1024];
    let mut stop_seen: Option<Instant> = None;
    loop {
        let mut pfd = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 50) };
        if stop.load(Ordering::SeqCst) {
            let since = *stop_seen.get_or_insert_with(Instant::now);
            if r == 0 || since.elapsed() > Duration::from_millis(400) {
                break;
            }
        }
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if r == 0 {
            continue;
        }
        let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) {
                continue;
            }
            break; // EIO: every writer of the pty is gone
        }
        if n == 0 {
            break;
        }
        let chunk = &buf[..n as usize];
        match sink {
            Sink::Stdout => {
                let mut o = io::stdout().lock();
                let _ = o.write_all(chunk);
                let _ = o.flush();
            }
            Sink::Stderr => {
                let mut o = io::stderr().lock();
                let _ = o.write_all(chunk);
                let _ = o.flush();
            }
            Sink::Discard => {}
        }
        let now = t0.elapsed().as_secs_f64() * 1000.0;
        split.feed(chunk, now, &store);
    }
    split.emit(&store);
}

struct Splitter {
    buf: Vec<u8>,
    start: Option<f64>,
    err: bool,
}

impl Splitter {
    fn feed(&mut self, data: &[u8], now: f64, store: &Mutex<Store>) {
        for &b in data {
            if self.start.is_none() {
                self.start = Some(now);
            }
            if b == b'\n' {
                self.emit(store);
            } else {
                self.buf.push(b);
                if self.buf.len() > 256 * 1024 {
                    self.emit(store);
                }
            }
        }
    }

    fn emit(&mut self, store: &Mutex<Store>) {
        let t = self.start.take().unwrap_or(0.0);
        if self.buf.is_empty() {
            return;
        }
        let raw = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        // Progress bars redraw with \r: keep what was visible last.
        let visible = raw.trim_end_matches('\r').rsplit('\r').next().unwrap_or("");
        let text = strip_ansi(visible);
        let text = text.trim_end();
        if text.trim().is_empty() {
            return;
        }
        let mut s = store.lock().unwrap();
        if s.lines.len() >= MAX_LINES {
            s.dropped += 1;
            return;
        }
        s.lines.push(OutputLine { t_ms: t, text: truncate(text, MAX_LINE_CHARS), err: self.err });
    }
}
