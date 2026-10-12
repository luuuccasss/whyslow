//! `whyslow compare before.json after.json`: what got faster, what got slower.

use crate::analyze::Analysis;
use crate::model::Trace;
use crate::render_term::Style;
use crate::util::{fmt_delta, fmt_ms, truncate};
use std::collections::BTreeSet;
use std::fmt::Write;

pub fn render(bt: &Trace, ba: &Analysis, at: &Trace, aa: &Analysis, st: &Style, width: usize) -> String {
    let mut o = String::new();
    let pct = |b: f64, a: f64| if b > 0.0 { format!("{:+.0}%", (a - b) / b * 100.0) } else { "new".into() };
    let paint = |d: f64, s: String, base: f64| {
        // Ignore noise below 3% of the run or 50ms.
        if d.abs() < (base * 0.03).max(50.0) {
            st.dim(&s)
        } else if d < 0.0 {
            st.green(&s)
        } else {
            st.red(&s)
        }
    };
    let base = bt.wall_ms.max(at.wall_ms);
    let _ = writeln!(o, "\n  {}", st.bold("whyslow compare"));
    let _ = writeln!(o, "  {} {}", st.dim("before"), truncate(&bt.command.join(" "), width - 12));
    let _ = writeln!(o, "  {} {}\n", st.dim("after "), truncate(&at.command.join(" "), width - 12));
    let d = at.wall_ms - bt.wall_ms;
    let _ = writeln!(
        o,
        "  {:<14} {:>9} → {:<9} {}",
        "wall time",
        fmt_ms(bt.wall_ms),
        fmt_ms(at.wall_ms),
        paint(d, format!("{} ({})", fmt_delta(d), pct(bt.wall_ms, at.wall_ms)), base)
    );
    let d = aa.cpu_ms - ba.cpu_ms;
    let _ = writeln!(
        o,
        "  {:<14} {:>9} → {:<9} {}",
        "cpu time",
        fmt_ms(ba.cpu_ms),
        fmt_ms(aa.cpu_ms),
        paint(d, format!("{} ({})", fmt_delta(d), pct(ba.cpu_ms, aa.cpu_ms)), base)
    );
    let _ = writeln!(o, "  {:<14} {:>9} → {:<9}\n", "processes", bt.spans.len(), at.spans.len());

    let keys: BTreeSet<&str> = ba.groups.iter().chain(aa.groups.iter()).map(|g| g.key.as_str()).collect();
    let mut rows: Vec<(&str, f64, f64, u32, u32)> = keys
        .into_iter()
        .map(|k| {
            let b = ba.groups.iter().find(|g| g.key == k);
            let a = aa.groups.iter().find(|g| g.key == k);
            (
                k,
                b.map(|g| g.self_ms).unwrap_or(0.0),
                a.map(|g| g.self_ms).unwrap_or(0.0),
                b.map(|g| g.count).unwrap_or(0),
                a.map(|g| g.count).unwrap_or(0),
            )
        })
        .filter(|r| r.1.max(r.2) >= base * 0.005)
        .collect();
    rows.sort_by(|x, y| (y.2 - y.1).abs().total_cmp(&(x.2 - x.1).abs()));
    let key_w = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(8).clamp(8, 28);
    let _ = writeln!(o, "  {}", st.dim(&format!("{:<key_w$}  {:>9}  {:>9}  {:>9}  {:>6}  runs", "tool", "before", "after", "delta", "")));
    for (k, b, a, bn, an) in rows.into_iter().take(15) {
        let d = a - b;
        let runs = if bn == an { format!("×{an}") } else { format!("×{bn} → ×{an}") };
        let _ = writeln!(
            o,
            "  {:<key_w$}  {:>9}  {:>9}  {}  {:>6}  {}",
            truncate(k, key_w),
            if bn > 0 { fmt_ms(b) } else { "-".into() },
            if an > 0 { fmt_ms(a) } else { "-".into() },
            paint(d, format!("{:>9}", fmt_delta(d)), base),
            if bn > 0 && an > 0 { pct(b, a) } else if an == 0 { "gone".into() } else { "new".into() },
            st.dim(&runs)
        );
    }
    o
}
