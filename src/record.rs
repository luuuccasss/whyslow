use crate::capture::{self, ChildIo};
use crate::model::{Span, Trace, FORMAT_VERSION};
use crate::util::{cpu_count, now_unix_ms};
use anyhow::{bail, Result};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Backend {
    /// ptrace on Linux, polling elsewhere or when ptrace is not allowed
    Auto,
    /// Exact process events through ptrace (Linux only)
    Ptrace,
    /// Periodic process-table snapshots (any OS, misses very short processes)
    Poll,
}

/// What a backend hands back once the root command exited.
pub struct Outcome {
    pub spans: Vec<Span>,
    pub ticks: Vec<f32>,
    pub sample_ms: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

pub enum BackendError {
    /// ptrace is not permitted here (seccomp, Yama, nested tracer…).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Denied(std::io::Error),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for BackendError {
    fn from(e: anyhow::Error) -> Self {
        BackendError::Other(e)
    }
}

pub fn spawn_error(cmd: &str, e: std::io::Error) -> anyhow::Error {
    match e.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("command not found: {cmd}"),
        std::io::ErrorKind::PermissionDenied => anyhow::anyhow!("permission denied: {cmd}"),
        _ => anyhow::anyhow!("cannot start {cmd}: {e}"),
    }
}

pub fn record(cmd: &[String], backend: Backend, quiet: bool) -> Result<Trace> {
    crate::signals::install();
    let t0 = Instant::now();
    let started_unix_ms = now_unix_ms();
    let (io, cap) = capture::start(t0, quiet)?;
    let mut io: Option<ChildIo> = Some(io);

    let (outcome, used) = match backend {
        Backend::Poll => (finish(crate::poll_backend::run(cmd, &mut io, t0))?, "poll"),
        Backend::Auto | Backend::Ptrace => {
            #[cfg(target_os = "linux")]
            {
                match crate::ptrace_backend::run(cmd, &mut io, t0) {
                    Ok(o) => (o, "ptrace"),
                    Err(BackendError::Denied(e)) if backend == Backend::Auto => {
                        eprintln!("whyslow: ptrace is not permitted here ({e}); falling back to polling");
                        (finish(crate::poll_backend::run(cmd, &mut io, t0))?, "poll")
                    }
                    Err(BackendError::Denied(e)) => bail!("ptrace is not permitted here: {e}"),
                    Err(BackendError::Other(e)) => return Err(e),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                if backend == Backend::Ptrace {
                    bail!("the ptrace backend is only available on Linux");
                }
                (finish(crate::poll_backend::run(cmd, &mut io, t0))?, "poll")
            }
        }
    };
    drop(io);
    let (output, output_dropped) = cap.finish();

    let mut outcome = outcome;
    let mut output = output;
    // Time zero is the moment the command started.
    let origin = outcome.spans[0].start_ms;
    for s in &mut outcome.spans {
        s.start_ms -= origin;
        s.end_ms -= origin;
    }
    for t in &mut outcome.ticks {
        *t -= origin as f32;
    }
    for l in &mut output {
        l.t_ms = (l.t_ms - origin).max(0.0);
    }
    let wall_ms = outcome.spans[0].dur();
    Ok(Trace {
        version: FORMAT_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        command: cmd.to_vec(),
        cwd: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        os: std::env::consts::OS.to_string(),
        cpus: cpu_count(),
        backend: used.to_string(),
        started_unix_ms,
        wall_ms,
        exit_code: outcome.exit_code,
        signal: outcome.signal,
        interrupted: crate::signals::interrupted(),
        sample_ms: outcome.sample_ms,
        ticks: outcome.ticks,
        spans: outcome.spans,
        output,
        output_dropped,
    })
}

fn finish(r: std::result::Result<Outcome, BackendError>) -> Result<Outcome> {
    match r {
        Ok(o) => Ok(o),
        Err(BackendError::Denied(e)) => Err(e.into()),
        Err(BackendError::Other(e)) => Err(e),
    }
}
