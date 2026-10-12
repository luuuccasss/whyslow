//! File outputs: raw recording (JSON), Chrome trace events (Perfetto,
//! chrome://tracing, speedscope) and the self-contained HTML report.

use crate::analyze::Analysis;
use crate::model::Trace;
use crate::util::shell_join;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;

pub fn write_json(t: &Trace, path: &Path) -> Result<()> {
    let data = serde_json::to_vec(t)?;
    std::fs::write(path, data).with_context(|| format!("cannot write {}", path.display()))
}

pub fn chrome_trace(t: &Trace, a: &Analysis) -> Value {
    let mut ev: Vec<Value> = Vec::with_capacity(t.spans.len() + t.ticks.len() + 8);
    ev.push(json!({"ph": "M", "name": "process_name", "pid": 1, "args": {"name": format!("whyslow: {}", t.command.join(" "))}}));
    ev.push(json!({"ph": "M", "name": "thread_name", "pid": 1, "tid": 0, "args": {"name": "output"}}));
    for r in 0..a.row_count {
        ev.push(json!({"ph": "M", "name": "thread_name", "pid": 1, "tid": r + 1, "args": {"name": format!("row {}", r + 1)}}));
    }
    for s in &t.spans {
        let i = s.id as usize;
        ev.push(json!({
            "ph": "X",
            "name": a.labels[i],
            "cat": a.keys[i],
            "pid": 1,
            "tid": a.rows[i] + 1,
            "ts": s.start_ms * 1000.0,
            "dur": s.dur() * 1000.0,
            "args": {
                "command": shell_join(&s.argv),
                "pid": s.pid,
                "self_ms": a.self_ms[i],
                "cpu_ms": s.cpu_ms(),
                "max_rss_kb": s.max_rss_kb,
                "exit_code": s.exit_code,
            }
        }));
    }
    // CPU usage counter.
    let mut per_tick = vec![0f32; t.ticks.len()];
    for s in &t.spans {
        for &(k, pct) in &s.samples {
            if let Some(v) = per_tick.get_mut(k as usize) {
                *v += pct / 100.0;
            }
        }
    }
    for (k, &time) in t.ticks.iter().enumerate() {
        ev.push(json!({"ph": "C", "name": "CPU cores", "pid": 1, "ts": time as f64 * 1000.0, "args": {"cores": per_tick[k]}}));
    }
    for l in t.output.iter().take(5000) {
        ev.push(json!({"ph": "i", "s": "t", "name": l.text, "pid": 1, "tid": 0, "ts": l.t_ms * 1000.0}));
    }
    json!({"traceEvents": ev, "displayTimeUnit": "ms"})
}

pub fn write_chrome(t: &Trace, a: &Analysis, path: &Path) -> Result<()> {
    let data = serde_json::to_vec(&chrome_trace(t, a))?;
    std::fs::write(path, data).with_context(|| format!("cannot write {}", path.display()))
}

const TEMPLATE: &str = include_str!("../assets/report.html");

pub fn html(t: &Trace, a: &Analysis) -> Result<String> {
    let data = serde_json::to_string(&json!({"trace": t, "analysis": a}))?;
    // Safe inside <script>: no "</script>" or HTML comment openers.
    let data = data.replace("</", "<\\/").replace("<!--", "<\\!--");
    Ok(TEMPLATE.replace("/*__WHYSLOW_DATA__*/null", &data))
}

pub fn write_html(t: &Trace, a: &Analysis, path: &Path) -> Result<()> {
    std::fs::write(path, html(t, a)?).with_context(|| format!("cannot write {}", path.display()))
}

pub fn open_in_browser(path: &Path) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
