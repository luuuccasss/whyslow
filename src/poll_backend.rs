//! Portable backend: snapshots the process table every 20ms and follows the
//! command's descendants. Processes shorter than one snapshot can be missed and
//! start/end times are ±10ms, but it needs no special permission.

use crate::capture::ChildIo;
use crate::model::Span;
use crate::record::{spawn_error, BackendError, Outcome};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const POLL_MS: u64 = 20;
const SAMPLE_MS: u64 = 100;

fn ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1000.0
}

fn argv_of(p: &sysinfo::Process) -> Vec<String> {
    p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

pub fn run(cmd: &[String], io: &mut Option<ChildIo>, t0: Instant) -> Result<Outcome, BackendError> {
    let (out, err) = io.as_ref().expect("child io").stdio().map_err(|e| BackendError::Other(e.into()))?;
    let start = ms(t0);
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| BackendError::Other(spawn_error(&cmd[0], e)))?;
    *io = None;
    let root = child.id() as i32;

    let mut root_span = Span::new(0, None, root, cmd.to_vec(), start);
    root_span.cwd = std::env::current_dir().ok().map(|p| p.display().to_string());
    let mut spans = vec![root_span];
    let mut live: HashMap<i32, u32> = HashMap::from([(root, 0)]);
    let mut last_cpu: HashMap<i32, u64> = HashMap::new();
    let mut ticks: Vec<f32> = Vec::new();
    let mut last_sample = Instant::now();
    let mut sys = System::new();
    let kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_cmd(UpdateKind::Always)
        .with_cwd(UpdateKind::OnlyIfNotSet)
        .with_disk_usage()
        .without_tasks();

    let (exit_code, signal) = loop {
        if let Some(sig) = crate::signals::take_pending() {
            unsafe { libc::kill(root, sig) };
        }
        let exited = child.try_wait().ok().flatten();
        let now = ms(t0);
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);

        // Descendants of the root, parents first.
        let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();
        for (pid, p) in sys.processes() {
            if let Some(pp) = p.parent() {
                kids.entry(pp.as_u32() as i32).or_default().push(pid.as_u32() as i32);
            }
        }
        let mut order = Vec::new();
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            order.push(p);
            if let Some(k) = kids.get(&p) {
                stack.extend(k.iter().copied());
            }
        }
        let seen: HashSet<i32> = order.iter().copied().collect();

        let sample_dt = last_sample.elapsed();
        let take_sample = sample_dt >= Duration::from_millis(SAMPLE_MS);
        let tick = ticks.len() as u32;
        if take_sample {
            ticks.push(now as f32);
            last_sample = Instant::now();
        }

        for &pid in &order {
            let Some(p) = sys.process(Pid::from_u32(pid as u32)) else { continue };
            let id = match live.get(&pid) {
                Some(&id) => id,
                None => {
                    let parent = p.parent().and_then(|pp| live.get(&(pp.as_u32() as i32)).copied());
                    let id = spans.len() as u32;
                    // Born somewhere since the previous snapshot.
                    let mut s = Span::new(id, parent, pid, argv_of(p), (now - POLL_MS as f64 / 2.0).max(start));
                    s.cwd = p.cwd().map(|c| c.display().to_string());
                    spans.push(s);
                    live.insert(pid, id);
                    id
                }
            };
            let s = &mut spans[id as usize];
            let argv = argv_of(p);
            if !argv.is_empty() && argv != s.argv {
                if pid == root && s.prev_argv.is_empty() && s.end_ms == s.start_ms {
                    // The root's first look: keep the argv the user typed.
                } else {
                    let old = std::mem::replace(&mut s.argv, argv);
                    s.prev_argv.push(old);
                }
            }
            s.end_ms = now;
            s.max_rss_kb = s.max_rss_kb.max(p.memory() / 1024);
            let du = p.disk_usage();
            s.read_bytes = du.total_read_bytes;
            s.write_bytes = du.total_written_bytes;
            let cpu = p.accumulated_cpu_time();
            if cpu > 0 {
                // Only the total is known: report it all as user time.
                s.user_ms = cpu as f64;
            }
            if take_sample {
                let before = last_cpu.insert(pid, cpu).unwrap_or(0);
                let dt = sample_dt.as_secs_f64() * 1000.0;
                let pct = (cpu.saturating_sub(before)) as f64 / dt * 100.0;
                if pct > 0.0 {
                    s.samples.push((tick, pct as f32));
                }
            }
        }
        // Gone since the previous snapshot.
        live.retain(|pid, id| {
            if seen.contains(pid) || *pid == root {
                return true;
            }
            spans[*id as usize].end_ms = now - POLL_MS as f64 / 2.0;
            false
        });

        if let Some(st) = exited {
            use std::os::unix::process::ExitStatusExt;
            spans[0].end_ms = now;
            break (st.code(), st.signal());
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    };

    let root_end = spans[0].end_ms;
    for (&pid, &id) in &live {
        if pid != root {
            let s = &mut spans[id as usize];
            s.end_ms = root_end;
            s.outlived = true;
        }
    }
    Ok(Outcome { spans, ticks, sample_ms: SAMPLE_MS as u32, exit_code, signal })
}
