//! The report printed after the command finishes (on stderr, so the
//! command's own stdout stays clean for pipes).

use crate::analyze::{Analysis, Level};
use crate::model::Trace;
use crate::util::{fmt_kb, fmt_ms, truncate};
use std::collections::HashSet;
use std::fmt::Write;

pub struct Style {
    pub color: bool,
}

impl Style {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    pub fn accent(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    pub fn warn(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    /// Render `backtick` spans in bold (or keep the backticks without color).
    pub fn inline(&self, s: &str) -> String {
        if !self.color {
            return s.to_string();
        }
        let mut out = String::new();
        for (i, part) in s.split('`').enumerate() {
            if i % 2 == 1 {
                out.push_str(&self.bold(part))
            } else {
                out.push_str(part)
            }
        }
        out
    }
}

fn visible_len(s: &str) -> usize {
    crate::util::strip_ansi(s).chars().count()
}

fn pad(s: &str, w: usize) -> String {
    let l = visible_len(s);
    if l >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - l))
    }
}

fn lpad(s: &str, w: usize) -> String {
    let l = visible_len(s);
    if l >= w {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(w - l))
    }
}

/// Greedy word wrap of plain text containing `code` marks.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split(' ') {
        let cur = lines.last_mut().unwrap();
        let wl = word.replace('`', "").chars().count();
        let cl = cur.replace('`', "").chars().count();
        if cl > 0 && cl + 1 + wl > width {
            lines.push(word.to_string());
        } else {
            if cl > 0 {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    // Keep `code` spans balanced on every line.
    let mut open = false;
    for l in &mut lines {
        if open {
            l.insert(0, '`');
        }
        if l.matches('`').count() % 2 == 1 {
            l.push('`');
            open = !open;
        }
    }
    lines
}

fn nice_step(range_ms: f64, max_ticks: usize) -> f64 {
    let raw = range_ms / max_ticks.max(1) as f64;
    let mag = 10f64.powf(raw.max(1e-3).log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if m * mag >= raw {
            return m * mag;
        }
    }
    10.0 * mag
}

fn axis_label(ms: f64, step: f64) -> String {
    if step >= 60_000.0 {
        format!("{}m", (ms / 60_000.0).round())
    } else if step >= 1000.0 {
        format!("{}s", (ms / 1000.0).round())
    } else if ms == 0.0 {
        "0".into()
    } else if step >= 100.0 {
        format!("{:.1}s", ms / 1000.0)
    } else if step >= 1.0 {
        format!("{}ms", ms.round())
    } else {
        format!("{ms:.1}ms")
    }
}

pub fn render(t: &Trace, a: &Analysis, st: &Style, width: usize, max_rows: usize) -> String {
    let width = width.clamp(60, 160);
    let wall = t.wall_ms.max(1e-3);
    let mut o = String::new();

    // Header
    let rule = "─".repeat(width.saturating_sub(10));
    let _ = writeln!(o, "\n{} {}", st.dim(&rule), st.bold("whyslow"));
    let cmd = truncate(&t.command.join(" "), width - 4);
    let _ = writeln!(o, "  {}", st.bold(&cmd));
    let status = match (t.exit_code, t.signal) {
        (Some(0), _) => st.green("exit 0"),
        (Some(c), _) => st.red(&format!("exit {c}")),
        (None, Some(s)) => st.red(&format!("killed by signal {s}")),
        _ => st.dim("exit ?"),
    };
    let mut meta = format!(
        "  {}  {}  {}  CPU {} ({:.1} of {} cores)  {} process{}",
        st.bold(&fmt_ms(t.wall_ms)),
        st.dim("·"),
        status,
        fmt_ms(a.cpu_ms),
        a.parallelism,
        t.cpus,
        t.spans.len(),
        if t.spans.len() == 1 { "" } else { "es" }
    );
    if t.interrupted {
        meta.push_str(&format!("  {}", st.warn("interrupted")));
    }
    if t.backend == "poll" {
        meta.push_str(&format!("  {}", st.dim("(polling: short processes may be missing)")));
    }
    let _ = writeln!(o, "{meta}\n");

    // Timeline rows: the longest spans plus their ancestors, in tree order.
    let min_dur = wall * 0.01;
    let mut by_dur: Vec<u32> = t.spans.iter().filter(|s| s.dur() >= min_dur || s.id == 0).map(|s| s.id).collect();
    by_dur.sort_by(|x, y| t.spans[*y as usize].dur().total_cmp(&t.spans[*x as usize].dur()));
    let mut chosen: HashSet<u32> = HashSet::new();
    for id in by_dur {
        if chosen.len() >= max_rows {
            break;
        }
        let mut cur = Some(id);
        while let Some(c) = cur {
            if !chosen.insert(c) {
                break;
            }
            cur = t.spans[c as usize].parent;
        }
    }
    struct Row {
        id: u32,
        prefix: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    fn walk(id: u32, a: &Analysis, chosen: &HashSet<u32>, prefix: &str, connector: &str, rows: &mut Vec<Row>) {
        rows.push(Row { id, prefix: format!("{prefix}{connector}") });
        // Selected descendants, skipping unselected intermediate processes.
        let mut kids: Vec<u32> = Vec::new();
        let mut stack: Vec<u32> = a.children[id as usize].iter().rev().copied().collect();
        while let Some(k) = stack.pop() {
            if chosen.contains(&k) {
                kids.push(k);
            } else {
                stack.extend(a.children[k as usize].iter().rev());
            }
        }
        let child_prefix = format!(
            "{prefix}{}",
            match connector {
                "" => "",
                "├─ " => "│  ",
                _ => "   ",
            }
        );
        for (i, &k) in kids.iter().enumerate() {
            let last = i + 1 == kids.len();
            walk(k, a, chosen, &child_prefix, if last { "└─ " } else { "├─ " }, rows);
        }
    }
    for s in t.spans.iter().filter(|s| s.parent.is_none() && chosen.contains(&s.id)) {
        walk(s.id, a, &chosen, "", "", &mut rows);
    }

    let label_w = rows
        .iter()
        .map(|r| r.prefix.chars().count() + a.labels[r.id as usize].chars().count())
        .max()
        .unwrap_or(10)
        .clamp(16, (width / 3).max(16));
    let dur_w = 8;
    let cols = width.saturating_sub(label_w + dur_w + 6).clamp(20, 100);
    let step = nice_step(wall, cols / 10);

    let _ = writeln!(o, "  {}", st.bold("TIMELINE"));
    // Axis
    let mut axis = vec![' '; cols + 8];
    let mut tick = 0.0;
    while tick <= wall + 1e-6 {
        let c = ((tick / wall) * cols as f64).round() as usize;
        let lab = axis_label(tick, step);
        if c + lab.len() <= axis.len()
            && axis[c.saturating_sub(1)..(c + lab.len()).min(axis.len())].iter().all(|&x| x == ' ')
        {
            for (i, ch) in lab.chars().enumerate() {
                axis[c + i] = ch;
            }
        }
        tick += step;
    }
    let axis: String = axis.into_iter().collect();
    let _ = writeln!(o, "  {}  {}", " ".repeat(label_w + dur_w), st.dim(axis.trim_end()));

    for r in &rows {
        let s = &t.spans[r.id as usize];
        let name = truncate(&a.labels[r.id as usize], label_w.saturating_sub(r.prefix.chars().count()).max(4));
        let left = if r.prefix.is_empty() { name } else { format!("{}{}", st.dim(&r.prefix), name) };
        // 0 = empty, 1 = busy, 2 = idle
        let mut cells = vec![0u8; cols];
        let slice = wall / cols as f64;
        let mut mid_drawn = false;
        let idle = &a.idle[r.id as usize];
        for (c, cell) in cells.iter_mut().enumerate() {
            let (c0, c1) = (c as f64 * slice, (c + 1) as f64 * slice);
            let ov = (s.end_ms.min(c1) - s.start_ms.max(c0)).max(0.0);
            let mid = (s.start_ms + s.end_ms) / 2.0;
            let tiny = s.dur() < slice && !mid_drawn && mid >= c0 && mid < c1 + if c + 1 == cols { 1.0 } else { 0.0 };
            if ov >= slice * 0.5 || tiny {
                mid_drawn |= tiny;
                let idle_ov: f64 = idle.iter().map(|iv| (iv[1].min(c1) - iv[0].max(c0)).max(0.0)).sum();
                *cell = if idle_ov >= ov.max(1e-9) * 0.5 && !tiny { 2 } else { 1 };
            }
        }
        while cells.last() == Some(&0) {
            cells.pop();
        }
        let mut bar = String::new();
        let mut i = 0;
        while i < cells.len() {
            let j = (i..cells.len()).find(|&j| cells[j] != cells[i]).unwrap_or(cells.len());
            let n = j - i;
            match cells[i] {
                1 => bar.push_str(&st.accent(&"█".repeat(n))),
                2 => bar.push_str(&st.dim(&"░".repeat(n))),
                _ => bar.push_str(&" ".repeat(n)),
            }
            i = j;
        }
        let dur = lpad(&fmt_ms(s.dur()), dur_w);
        let _ = writeln!(o, "  {}{}  {}", pad(&left, label_w), st.dim(&dur), bar);
    }
    let hidden = t.spans.len().saturating_sub(rows.len());
    let mut legend = format!("{} busy  {} idle (nothing under it using CPU)", st.accent("█"), st.dim("░"));
    if hidden > 0 {
        legend.push_str(&format!("   {}", st.dim(&format!("+{hidden} shorter processes not shown"))));
    }
    let _ = writeln!(o, "  {}  {}\n", " ".repeat(label_w + dur_w), legend);

    // Where the time went
    let _ = writeln!(o, "  {}", st.bold("WHERE THE TIME WENT"));
    let key_w = a.groups.iter().take(8).map(|g| g.key.chars().count()).max().unwrap_or(8).clamp(8, 28);
    let _ = writeln!(
        o,
        "  {}",
        st.dim(&format!(
            "{}  {:>8}  {:>5}  {:<20}  {:>6}  {:>8}  {:>8}",
            pad("tool", key_w),
            "time",
            "share",
            "",
            "runs",
            "cpu",
            "peak mem"
        ))
    );
    let shown: Vec<_> = a.groups.iter().filter(|g| g.self_ms >= wall * 0.005).take(8).collect();
    for g in &shown {
        let share = g.self_ms / wall;
        let filled = (share * 20.0).round() as usize;
        let bar = format!("{}{}", st.accent(&"■".repeat(filled)), " ".repeat(20 - filled.min(20)));
        let _ = writeln!(
            o,
            "  {}  {:>8}  {:>4.0}%  {}  {:>6}  {:>8}  {:>8}",
            pad(&truncate(&g.key, key_w), key_w),
            fmt_ms(g.self_ms),
            share * 100.0,
            bar,
            format!("×{}", g.count),
            fmt_ms(g.cpu_ms),
            if g.peak_rss_kb > 0 { fmt_kb(g.peak_rss_kb) } else { "-".into() }
        );
    }
    let rest: f64 = a.groups.iter().skip(shown.len()).map(|g| g.self_ms).sum();
    let rest_n = a.groups.len() - shown.len();
    if rest_n > 0 {
        let _ = writeln!(
            o,
            "  {}",
            st.dim(&format!(
                "{}  {:>8}  {:>4.0}%  ({rest_n} more)",
                pad("other", key_w),
                fmt_ms(rest),
                rest / wall * 100.0
            ))
        );
    }
    let _ = writeln!(o);

    // Insights
    if !a.insights.is_empty() {
        let _ = writeln!(o, "  {}", st.bold("WHY IT'S SLOW"));
        for i in a.insights.iter().take(6) {
            let mark = match i.level {
                Level::Warn => st.warn("▲"),
                Level::Tip => st.accent("›"),
                Level::Info => st.dim("·"),
            };
            for (n, line) in wrap(&i.text, width - 8).iter().enumerate() {
                let _ = writeln!(o, "  {} {}", if n == 0 { mark.clone() } else { " ".into() }, st.inline(line));
            }
            if let Some(h) = &i.hint {
                for line in wrap(h, width - 10) {
                    let _ = writeln!(o, "    {}", st.inline(&line));
                }
            }
        }
        if a.insights.len() > 6 {
            let _ = writeln!(o, "  {}", st.dim(&format!("+{} more in the HTML report (--html)", a.insights.len() - 6)));
        }
    } else {
        let _ = writeln!(o, "  {}", st.dim("Nothing stands out: no single step dominates and nothing sat idle."));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_code_balanced() {
        let lines = wrap("the `very long command name` ran", 12);
        for l in &lines {
            assert_eq!(l.matches('`').count() % 2, 0, "{lines:?}");
        }
    }
}
