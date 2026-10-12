//! Linux backend: the command runs under ptrace with fork/clone/exec/exit
//! events only (no syscall tracing), so every process is seen, even ones that
//! live for a millisecond, at a cost of a few microseconds per process.
//! A sampler thread reads /proc every 100ms for the CPU timeline.

use crate::capture::ChildIo;
use crate::model::Span;
use crate::record::{spawn_error, BackendError, Outcome};
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SAMPLE_MS: u64 = 100;

const OPTIONS: libc::c_int = libc::PTRACE_O_TRACEFORK
    | libc::PTRACE_O_TRACEVFORK
    | libc::PTRACE_O_TRACECLONE
    | libc::PTRACE_O_TRACEEXEC
    | libc::PTRACE_O_TRACEEXIT;

fn ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

pub fn run(cmd: &[String], io: &mut Option<ChildIo>, t0: Instant) -> Result<Outcome, BackendError> {
    let (out, err) = io.as_ref().expect("child io").stdio().map_err(|e| BackendError::Other(e.into()))?;
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdout(out).stderr(err);
    unsafe {
        c.pre_exec(|| {
            if libc::ptrace(libc::PTRACE_TRACEME, 0, 0usize, 0usize) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start = ms(t0);
    let child = match c.spawn() {
        Ok(c) => c,
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => return Err(BackendError::Denied(e)),
        Err(e) => return Err(BackendError::Other(spawn_error(&cmd[0], e))),
    };
    // Our copies of the command's stdout/stderr must go, or the reader never sees EOF.
    *io = None;
    let root = child.id() as i32;
    drop(child);

    let mut root_span = Span::new(0, None, root, cmd.to_vec(), start);
    root_span.cwd = std::env::current_dir().ok().map(|p| p.display().to_string());

    let live: Arc<Mutex<HashMap<i32, u32>>> = Arc::new(Mutex::new(HashMap::from([(root, 0)])));
    let sampler = Sampler::start(live.clone(), t0);

    let mut t = Tracer {
        spans: vec![root_span],
        procs: HashMap::from([(root, 0)]),
        threads: HashMap::new(),
        expect_stop: HashSet::new(),
        early_stop: HashSet::new(),
        live,
    };
    let (exit_code, signal) = t.run(root, t0);

    let root_end = t.spans[0].end_ms;
    for (pid, id) in std::mem::take(&mut t.procs) {
        let s = &mut t.spans[id as usize];
        read_stats(s, pid);
        s.end_ms = root_end.max(s.start_ms);
        s.outlived = true;
    }
    let (ticks, samples) = sampler.stop();
    for (tick, span, pct) in samples {
        if let Some(s) = t.spans.get_mut(span as usize) {
            s.samples.push((tick, pct));
        }
    }
    // Leftover tracees (daemons) are detached by the kernel when we exit.
    Ok(Outcome { spans: t.spans, ticks, sample_ms: SAMPLE_MS as u32, exit_code, signal })
}

struct Tracer {
    spans: Vec<Span>,
    /// Live traced processes (thread-group leaders) → span.
    procs: HashMap<i32, u32>,
    /// Traced non-leader threads → their process.
    threads: HashMap<i32, i32>,
    /// New tracees whose initial SIGSTOP has not been seen yet.
    expect_stop: HashSet<i32>,
    /// Initial SIGSTOPs that arrived before the parent's fork event.
    early_stop: HashSet<i32>,
    live: Arc<Mutex<HashMap<i32, u32>>>,
}

impl Tracer {
    fn run(&mut self, root: i32, t0: Instant) -> (Option<i32>, Option<i32>) {
        let mut root_ready = false;
        loop {
            if let Some(sig) = crate::signals::take_pending() {
                unsafe { libc::kill(root, sig) };
            }
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL) };
            if pid == -1 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                // ECHILD: nothing left to wait for.
                self.spans[0].end_ms = ms(t0);
                return (None, None);
            }
            let now = ms(t0);

            if libc::WIFSTOPPED(status) {
                let sig = libc::WSTOPSIG(status);
                let event = (status >> 16) & 0xff;
                if event != 0 {
                    self.on_event(pid, event, now);
                    cont(pid, 0);
                    continue;
                }
                if pid == root && !root_ready && sig == libc::SIGTRAP {
                    // First stop, right after the command's execve.
                    unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, root, 0usize, OPTIONS as usize) };
                    root_ready = true;
                    cont(pid, 0);
                    continue;
                }
                if sig == libc::SIGSTOP {
                    if self.expect_stop.remove(&pid) {
                        cont(pid, 0);
                        continue;
                    }
                    if !self.procs.contains_key(&pid) && !self.threads.contains_key(&pid) {
                        self.early_stop.insert(pid);
                        cont(pid, 0);
                        continue;
                    }
                }
                if matches!(sig, libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU) && is_group_stop(pid) {
                    cont(pid, 0);
                    continue;
                }
                // A real signal for the program: deliver it.
                cont(pid, sig);
                continue;
            }

            let (code, sig) = if libc::WIFEXITED(status) {
                (Some(libc::WEXITSTATUS(status)), None)
            } else if libc::WIFSIGNALED(status) {
                (None, Some(libc::WTERMSIG(status)))
            } else {
                continue;
            };
            if self.threads.remove(&pid).is_some() {
                continue;
            }
            if let Some(id) = self.procs.remove(&pid) {
                self.live.lock().unwrap().remove(&pid);
                let s = &mut self.spans[id as usize];
                s.end_ms = now;
                s.exit_code = code;
                s.signal = sig;
            }
            if pid == root {
                return (code, sig);
            }
        }
    }

    fn on_event(&mut self, pid: i32, event: i32, now: f64) {
        match event {
            libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                let new = event_msg(pid) as i32;
                if new <= 0 {
                    return;
                }
                if event == libc::PTRACE_EVENT_CLONE {
                    if let Some(tgid) = tgid_of(new).filter(|&g| g != new) {
                        self.threads.insert(new, tgid);
                        if !self.early_stop.remove(&new) {
                            self.expect_stop.insert(new);
                        }
                        return;
                    }
                }
                let owner = if self.procs.contains_key(&pid) { pid } else { self.threads.get(&pid).copied().unwrap_or(pid) };
                let parent = self.procs.get(&owner).copied();
                let id = self.spans.len() as u32;
                let mut s = match parent {
                    Some(p) => {
                        let ps = &self.spans[p as usize];
                        let mut s = Span::new(id, Some(p), new, ps.argv.clone(), now);
                        s.cwd = ps.cwd.clone();
                        s
                    }
                    None => Span::new(id, None, new, read_cmdline(new).unwrap_or_default(), now),
                };
                s.forked = true;
                self.spans.push(s);
                self.procs.insert(new, id);
                self.live.lock().unwrap().insert(new, id);
                if !self.early_stop.remove(&new) {
                    self.expect_stop.insert(new);
                }
            }
            libc::PTRACE_EVENT_EXEC => {
                let former = event_msg(pid) as i32;
                if former != pid {
                    self.threads.remove(&former);
                }
                let Some(&id) = self.procs.get(&pid) else { return };
                let s = &mut self.spans[id as usize];
                if let Some(argv) = read_cmdline(pid) {
                    if s.forked {
                        s.argv = argv;
                        s.forked = false;
                    } else {
                        let old = std::mem::replace(&mut s.argv, argv);
                        s.prev_argv.push(old);
                    }
                }
                if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                    s.cwd = Some(p.display().to_string());
                }
            }
            libc::PTRACE_EVENT_EXIT => {
                // The process still exists here: last chance to read its counters.
                if let Some(&id) = self.procs.get(&pid) {
                    read_stats(&mut self.spans[id as usize], pid);
                }
            }
            _ => {}
        }
    }
}

fn cont(pid: i32, sig: i32) {
    unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0usize, sig as usize) };
}

fn event_msg(pid: i32) -> libc::c_ulong {
    let mut msg: libc::c_ulong = 0;
    unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, pid, 0usize, &mut msg as *mut libc::c_ulong) };
    msg
}

/// Group-stops (SIGSTOP/SIGTSTP actually stopping the process) have no siginfo.
fn is_group_stop(pid: i32) -> bool {
    unsafe {
        let mut si: libc::siginfo_t = std::mem::zeroed();
        libc::ptrace(libc::PTRACE_GETSIGINFO, pid, 0usize, &mut si as *mut libc::siginfo_t) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
    }
}

fn tgid_of(tid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    status.lines().find_map(|l| l.strip_prefix("Tgid:")).and_then(|v| v.trim().parse().ok())
}

fn read_cmdline(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>();
    let mut argv = argv;
    while argv.last().is_some_and(|a| a.is_empty()) {
        argv.pop();
    }
    (!argv.is_empty()).then_some(argv)
}

fn clk_tck() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as f64 } else { 100.0 }
}

/// utime + stime of the whole process, in clock ticks.
fn cpu_ticks(pid: i32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses: split after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(11)?.parse().ok()?, f.get(12)?.parse().ok()?))
}

fn read_stats(s: &mut Span, pid: i32) {
    let tck = clk_tck();
    if let Some((u, sy)) = cpu_ticks(pid) {
        s.user_ms = u as f64 * 1000.0 / tck;
        s.sys_ms = sy as f64 * 1000.0 / tck;
    }
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        if let Some(v) = status.lines().find_map(|l| l.strip_prefix("VmHWM:")) {
            s.max_rss_kb = v.trim().trim_end_matches("kB").trim().parse().unwrap_or(s.max_rss_kb);
        }
    }
    if let Ok(io) = std::fs::read_to_string(format!("/proc/{pid}/io")) {
        for l in io.lines() {
            if let Some(v) = l.strip_prefix("read_bytes:") {
                s.read_bytes = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = l.strip_prefix("write_bytes:") {
                s.write_bytes = v.trim().parse().unwrap_or(0);
            }
        }
    }
}

type Samples = Vec<(u32, u32, f32)>;

struct Sampler {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<(Vec<f32>, Samples)>,
}

impl Sampler {
    fn start(live: Arc<Mutex<HashMap<i32, u32>>>, t0: Instant) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let tck = clk_tck();
            let mut ticks: Vec<f32> = Vec::new();
            let mut samples: Samples = Vec::new();
            let mut prev: HashMap<i32, (u32, u64)> = HashMap::new();
            let mut last = Instant::now();
            while !flag.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(SAMPLE_MS));
                let dt = last.elapsed().as_secs_f64();
                last = Instant::now();
                let snapshot: Vec<(i32, u32)> = live.lock().unwrap().iter().map(|(&p, &s)| (p, s)).collect();
                let tick = ticks.len() as u32;
                ticks.push(ms(t0) as f32);
                let mut seen = HashMap::with_capacity(snapshot.len());
                for (pid, span) in snapshot {
                    let Some((u, s)) = cpu_ticks(pid) else { continue };
                    let total = u + s;
                    let before = match prev.get(&pid) {
                        Some(&(sp, t)) if sp == span => t,
                        _ => 0, // a new process starts with zero CPU time
                    };
                    seen.insert(pid, (span, total));
                    let d = total.saturating_sub(before);
                    if d > 0 && dt > 0.0 {
                        samples.push((tick, span, (d as f64 / tck / dt * 100.0) as f32));
                    }
                }
                prev = seen;
            }
            (ticks, samples)
        });
        Sampler { stop, handle }
    }

    fn stop(self) -> (Vec<f32>, Samples) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join().unwrap_or_default()
    }
}
