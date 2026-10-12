//! Turns a raw trace into answers: who the time belongs to, where the command
//! was idle, and which patterns are worth fixing.

use crate::label::label;
use crate::model::{Span, Trace};
use crate::util::{fmt_kb, fmt_ms, shell_join, truncate};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Serialize, Clone, Debug)]
pub struct Group {
    pub key: String,
    /// Wall time attributed to this tool (shares sum to the total wall time).
    pub self_ms: f64,
    pub count: u32,
    pub cpu_ms: f64,
    pub wall_ms: f64,
    pub peak_rss_kb: u64,
    /// The span of this group with the most attributed time.
    pub top: u32,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warn,
    Tip,
    Info,
}

#[derive(Serialize, Clone, Debug)]
pub struct Insight {
    pub level: Level,
    /// One sentence; `backticks` mark command names.
    pub text: String,
    pub hint: Option<String>,
    pub spans: Vec<u32>,
    pub impact_ms: f64,
}

#[derive(Serialize, Debug)]
pub struct Analysis {
    pub labels: Vec<String>,
    pub keys: Vec<String>,
    /// Wall time attributed to each span: at every instant the processes doing
    /// the work (those without running children) share the clock.
    pub self_ms: Vec<f64>,
    pub depth: Vec<u32>,
    /// Flame-chart row of each span.
    pub rows: Vec<u32>,
    pub row_count: u32,
    /// Time ranges where the span and everything under it used < 10% of a core.
    pub idle: Vec<Vec<[f64; 2]>>,
    pub groups: Vec<Group>,
    pub insights: Vec<Insight>,
    pub cpu_ms: f64,
    pub parallelism: f64,
    #[serde(skip)]
    pub children: Vec<Vec<u32>>,
}

const IDLE_PCT: f32 = 10.0;

pub fn analyze(t: &Trace) -> Analysis {
    let n = t.spans.len();
    let spans = &t.spans;
    let wall = t.wall_ms.max(1e-3);

    let (labels, keys): (Vec<String>, Vec<String>) = spans
        .iter()
        .map(|s| {
            let l = label(&s.argv);
            if s.forked {
                (format!("{} (fork)", l.label), l.key)
            } else {
                (l.label, l.key)
            }
        })
        .unzip();

    let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
    for s in spans {
        if let Some(p) = s.parent {
            children[p as usize].push(s.id);
        }
    }
    for c in &mut children {
        c.sort_by(|a, b| spans[*a as usize].start_ms.total_cmp(&spans[*b as usize].start_ms));
    }
    let mut depth = vec![0u32; n];
    for s in spans {
        // Parents always come first, so their depth is already known.
        depth[s.id as usize] = s.parent.map(|p| depth[p as usize] + 1).unwrap_or(0);
    }

    let self_ms = attribute(spans);
    let (rows, row_count) = pack_rows(spans, &children, wall);
    let idle = idle_ranges(t, &children, wall);

    let cpu_ms: f64 = spans.iter().map(|s| s.cpu_ms()).sum();
    let mut by_key: HashMap<&str, Group> = HashMap::new();
    for s in spans {
        let i = s.id as usize;
        let g = by_key.entry(&keys[i]).or_insert_with(|| Group {
            key: keys[i].clone(),
            self_ms: 0.0,
            count: 0,
            cpu_ms: 0.0,
            wall_ms: 0.0,
            peak_rss_kb: 0,
            top: s.id,
        });
        g.self_ms += self_ms[i];
        g.count += 1;
        g.cpu_ms += s.cpu_ms();
        g.wall_ms += s.dur();
        g.peak_rss_kb = g.peak_rss_kb.max(s.max_rss_kb);
        if self_ms[i] > self_ms[g.top as usize] {
            g.top = s.id;
        }
    }
    let mut groups: Vec<Group> = by_key.into_values().collect();
    groups.sort_by(|a, b| b.self_ms.total_cmp(&a.self_ms).then(a.key.cmp(&b.key)));

    let mut a = Analysis {
        labels,
        keys,
        self_ms,
        depth,
        rows,
        row_count,
        idle,
        groups,
        insights: Vec::new(),
        cpu_ms,
        parallelism: cpu_ms / wall,
        children,
    };
    a.insights = insights(t, &a);
    a
}

/// Sweep over start/end events. Between two events, the running processes
/// that have no running child share the elapsed time equally.
fn attribute(spans: &[Span]) -> Vec<f64> {
    let n = spans.len();
    let mut ev: Vec<(f64, bool, u32)> = Vec::with_capacity(n * 2);
    for s in spans {
        let end = s.end_ms.max(s.start_ms);
        ev.push((s.start_ms, true, s.id));
        ev.push((end, false, s.id));
    }
    // Starts before ends at equal times so zero-length spans stay consistent.
    ev.sort_by(|a, b| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out = vec![0.0; n];
    let mut active = vec![false; n];
    let mut running_children = vec![0u32; n];
    let mut leaves: BTreeSet<u32> = BTreeSet::new();
    let mut prev_t = ev.first().map(|e| e.0).unwrap_or(0.0);
    for (time, is_start, id) in ev {
        let dt = time - prev_t;
        if dt > 0.0 && !leaves.is_empty() {
            let share = dt / leaves.len() as f64;
            for &l in &leaves {
                out[l as usize] += share;
            }
        }
        prev_t = time;
        let i = id as usize;
        let parent = spans[i].parent.map(|p| p as usize).filter(|&p| active[p]);
        if is_start {
            active[i] = true;
            if let Some(p) = parent {
                running_children[p] += 1;
                leaves.remove(&(p as u32));
            }
            if running_children[i] == 0 {
                leaves.insert(id);
            }
        } else {
            active[i] = false;
            leaves.remove(&id);
            if let Some(p) = parent {
                running_children[p] = running_children[p].saturating_sub(1);
                if running_children[p] == 0 {
                    leaves.insert(p as u32);
                }
            }
        }
    }
    out
}

/// Flame-chart layout: each span goes on the first row below its parent that
/// is free for its whole lifetime, so parallel children fan out.
fn pack_rows(spans: &[Span], children: &[Vec<u32>], wall: f64) -> (Vec<u32>, u32) {
    let n = spans.len();
    let mut rows = vec![0u32; n];
    let mut lanes: Vec<Vec<(f64, f64)>> = Vec::new();
    let min_w = wall * 0.002;
    let mut roots: Vec<u32> = spans.iter().filter(|s| s.parent.is_none()).map(|s| s.id).collect();
    roots.sort_by(|a, b| spans[*a as usize].start_ms.total_cmp(&spans[*b as usize].start_ms));
    let mut stack: Vec<u32> = roots.into_iter().rev().collect();
    while let Some(id) = stack.pop() {
        let s = &spans[id as usize];
        let (st, en) = (s.start_ms, s.end_ms.max(s.start_ms + min_w));
        let mut r = s.parent.map(|p| rows[p as usize] as usize + 1).unwrap_or(0);
        loop {
            if r >= lanes.len() {
                lanes.push(Vec::new());
            }
            let lane = &mut lanes[r];
            let idx = lane.partition_point(|iv| iv.1 <= st);
            if idx >= lane.len() || lane[idx].0 >= en {
                lane.insert(idx, (st, en));
                break;
            }
            r += 1;
        }
        rows[id as usize] = r as u32;
        stack.extend(children[id as usize].iter().rev());
    }
    (rows, lanes.len() as u32)
}

fn idle_ranges(t: &Trace, children: &[Vec<u32>], wall: f64) -> Vec<Vec<[f64; 2]>> {
    let mut out = vec![Vec::new(); t.spans.len()];
    if t.ticks.is_empty() {
        return out;
    }
    let tick_start = |k: usize| -> f64 {
        if k == 0 {
            (t.ticks[0] as f64 - t.sample_ms as f64).max(0.0)
        } else {
            t.ticks[k - 1] as f64
        }
    };
    let min_dur = (wall * 0.01).max(300.0);
    for s in &t.spans {
        if s.dur() < min_dur {
            continue;
        }
        // Ticks fully inside the span.
        let k0 = t.ticks.partition_point(|&x| (x as f64) < s.start_ms);
        let k1 = t.ticks.partition_point(|&x| (x as f64) <= s.end_ms);
        if k1 <= k0 {
            continue;
        }
        let mut sums = vec![0f32; k1 - k0];
        let mut stack = vec![s.id];
        while let Some(id) = stack.pop() {
            for &(k, pct) in &t.spans[id as usize].samples {
                let k = k as usize;
                if k >= k0 && k < k1 {
                    sums[k - k0] += pct;
                }
            }
            stack.extend(&children[id as usize]);
        }
        let mut ranges: Vec<[f64; 2]> = Vec::new();
        let mut run: Option<(f64, f64, u32)> = None;
        for (j, &sum) in sums.iter().enumerate() {
            let k = k0 + j;
            let (a, b) = (tick_start(k), t.ticks[k] as f64);
            let idle = sum < IDLE_PCT && a >= s.start_ms - 1.0;
            match (&mut run, idle) {
                (Some(r), true) => {
                    r.1 = b;
                    r.2 += 1;
                }
                (None, true) => run = Some((a, b, 1)),
                (Some(r), false) => {
                    if r.2 >= 3 {
                        ranges.push([r.0, r.1]);
                    }
                    run = None;
                }
                (None, false) => {}
            }
        }
        if let Some(r) = run.filter(|r| r.2 >= 3) {
            ranges.push([r.0, r.1]);
        }
        out[s.id as usize] = ranges;
    }
    out
}

fn idle_total(a: &Analysis, id: u32) -> f64 {
    a.idle[id as usize].iter().map(|r| r[1] - r[0]).sum()
}

fn avg_cores(s: &Span) -> f64 {
    if s.dur() <= 0.0 {
        0.0
    } else {
        s.cpu_ms() / s.dur()
    }
}

fn is_install(label: &str) -> bool {
    const P: &[&str] = &[
        "npm install",
        "npm ci",
        "npm i",
        "yarn install",
        "pnpm install",
        "pnpm i",
        "bun install",
        "pip install",
        "pip3 install",
        "poetry install",
        "uv sync",
        "uv pip",
        "bundle install",
        "composer install",
        "cargo fetch",
        "go mod",
        "mvn dependency",
        "apt-get install",
        "apt install",
    ];
    label == "yarn" || P.iter().any(|p| label == *p || label.starts_with(&format!("{p} ")))
}

fn has_arg(s: &Span, flags: &[&str]) -> bool {
    s.argv.iter().any(|a| flags.iter().any(|f| a == f || a.starts_with(&format!("{f}="))))
}

/// Does the tsconfig used by this tsc run enable incremental builds?
/// None when the file cannot be read (e.g. a recording opened on another machine).
fn tsconfig_incremental(s: &Span) -> Option<bool> {
    let cwd = std::path::PathBuf::from(s.cwd.as_ref()?);
    let proj = s
        .argv
        .iter()
        .position(|a| a == "-p" || a == "--project")
        .and_then(|i| s.argv.get(i + 1))
        .map(|p| cwd.join(p))
        .unwrap_or_else(|| cwd.join("tsconfig.json"));
    let path = if proj.is_dir() { proj.join("tsconfig.json") } else { proj };
    let text: String = std::fs::read_to_string(path).ok()?.chars().filter(|c| !c.is_whitespace()).collect();
    Some(text.contains("\"incremental\":true") || text.contains("\"composite\":true"))
}

fn tool_hint(key: &str, label: &str, s: &Span) -> Option<String> {
    if is_install(label) {
        return Some("In CI, cache the package manager's download cache (~/.npm, the pnpm store, ~/.cache/pip…) and install from the lockfile (`npm ci`, `pnpm install --frozen-lockfile`).".into());
    }
    match key {
        "tsc" if !has_arg(s, &["--incremental", "-b", "--build", "-w", "--watch"]) => match tsconfig_incremental(s) {
            Some(true) => None,
            Some(false) => Some("Turn on `\"incremental\": true` in tsconfig.json (or pass `--incremental`): tsc then reuses the previous run instead of starting from scratch.".into()),
            None => Some("If tsconfig.json doesn't set `\"incremental\": true`, add it: tsc then reuses the previous run instead of starting from scratch.".into()),
        },
        "eslint" if !has_arg(s, &["--cache"]) => Some("Add `--cache` so eslint skips files that haven't changed.".into()),
        "prettier" if !has_arg(s, &["--cache"]) => Some("Add `--cache` (Prettier ≥ 2.7) so unchanged files are skipped.".into()),
        "webpack" => Some("If it isn't on yet, webpack 5's persistent cache (`cache: { type: 'filesystem' }`) makes rebuilds much faster.".into()),
        "jest" if has_arg(s, &["--runInBand", "-i"]) => Some("jest ran with `--runInBand`: every test file ran in a single process.".into()),
        "ld" | "ld.bfd" | "ld.gold" | "collect2" => Some("A faster linker such as mold or lld usually cuts link time a lot.".into()),
        "docker" if label.starts_with("docker build") || label.starts_with("docker buildx") => {
            Some("The build runs inside the Docker daemon, so whyslow only sees the client waiting. Check layer caching: copy dependency manifests and install them before copying the source.".into())
        }
        "rustc" => {
            let flags = s.argv.join(" ");
            let one_unit = flags.contains("codegen-units=1");
            let lto = s.argv.windows(2).any(|w| w[0] == "-C" && (w[1] == "lto" || w[1] == "lto=fat" || w[1] == "lto=true"))
                || s.argv.iter().any(|a| a == "-Clto" || a == "-Clto=fat");
            match (one_unit, lto) {
                (true, true) => Some("This crate is built with `codegen-units=1` and fat LTO, which keeps it on a single core. Worth it for releases; for day-to-day builds use a profile without them.".into()),
                (true, false) => Some("This crate is built with `codegen-units=1`, which keeps code generation on a single core.".into()),
                (false, true) => Some("Fat LTO runs mostly on a single core; `lto = \"thin\"` is usually much faster for a similar result.".into()),
                _ => Some("`cargo build --timings` gives a per-crate breakdown of a Rust build.".into()),
            }
        }
        "cargo" => Some("`cargo build --timings` gives a per-crate breakdown of a Rust build.".into()),
        _ => None,
    }
}

fn code(s: &str) -> String {
    format!("`{}`", s.replace('`', "'"))
}

fn insights(t: &Trace, a: &Analysis) -> Vec<Insight> {
    let spans = &t.spans;
    let wall = t.wall_ms.max(1e-3);
    let mut out: Vec<Insight> = Vec::new();
    let mut covered: HashSet<u32> = HashSet::new();
    let lbl = |id: u32| a.labels[id as usize].as_str();

    // 1. One tool dominates.
    if let Some(g) = a.groups.first().filter(|g| g.self_ms / wall >= 0.3 && wall >= 500.0) {
        let top = &spans[g.top as usize];
        let name = if g.count == 1 { lbl(g.top).to_string() } else { g.key.clone() };
        let mut text =
            format!("{} accounts for {:.0}% of the time ({}", code(&name), g.self_ms / wall * 100.0, fmt_ms(g.self_ms));
        if g.count > 1 {
            text.push_str(&format!(" over {} runs", g.count));
        }
        text.push(')');
        let cores = avg_cores(top);
        let idle = idle_total(a, g.top);
        let top_self = a.self_ms[g.top as usize];
        // With many runs, name the one that matters when it dominates.
        let subject = if g.count > 1 && top_self >= g.self_ms * 0.5 {
            text.push_str(&format!(
                "; {} alone is {:.0}% ({})",
                code(lbl(g.top)),
                top_self / wall * 100.0,
                fmt_ms(top_self)
            ));
            true
        } else {
            g.count == 1
        };
        if subject && top.dur() >= 1000.0 {
            if idle >= top.dur() * 0.5 {
                text.push_str(&format!(
                    " and was idle for {} of it, waiting on the network, disk or another process",
                    fmt_ms(idle)
                ));
            } else if (0.75..=1.3).contains(&cores) && t.cpus > 1 {
                text.push_str(", busy on a single core");
            } else if cores > 1.5 {
                text.push_str(&format!(", using {cores:.1} cores on average"));
            }
        }
        text.push('.');
        covered.insert(g.top);
        out.push(Insight {
            level: Level::Warn,
            text,
            hint: tool_hint(&g.key, lbl(g.top), top),
            spans: vec![g.top],
            impact_ms: g.self_ms,
        });
    }

    // 2. Steps that ran one after another under the same parent.
    if t.cpus >= 2 {
        let sig = (wall * 0.05).max(1000.0);
        let mut best: Option<(f64, Vec<u32>, u32)> = None;
        for (p, kids) in a.children.iter().enumerate() {
            let big: Vec<u32> =
                kids.iter().copied().filter(|&k| spans[k as usize].dur() >= sig && !is_install(lbl(k))).collect();
            let mut chain: Vec<u32> = Vec::new();
            let mut flush = |chain: &mut Vec<u32>| {
                if chain.len() >= 2 {
                    let durs: Vec<f64> = chain.iter().map(|&k| spans[k as usize].dur()).collect();
                    let saving = durs.iter().sum::<f64>() - durs.iter().cloned().fold(0.0, f64::max);
                    let saturated = chain.iter().any(|&k| avg_cores(&spans[k as usize]) >= t.cpus as f64 * 0.6);
                    if !saturated && best.as_ref().is_none_or(|b| saving > b.0) {
                        best = Some((saving, chain.clone(), p as u32));
                    }
                }
                chain.clear();
            };
            for &k in &big {
                let s = &spans[k as usize];
                match chain.last() {
                    Some(&prev) if s.start_ms + 50.0 < spans[prev as usize].end_ms => {
                        flush(&mut chain);
                    }
                    _ => {}
                }
                chain.push(k);
            }
            flush(&mut chain);
        }
        if let Some((saving, chain, parent)) = best.filter(|b| b.0 >= (wall * 0.1).max(1000.0)) {
            let names: Vec<String> = chain.iter().map(|&k| code(lbl(k))).collect();
            out.push(Insight {
                level: Level::Warn,
                text: format!(
                    "{} ran one after another under {}. If they don't depend on each other, running them in parallel could save up to {}.",
                    names.join(" → "),
                    code(&truncate(lbl(parent), 40)),
                    fmt_ms(saving)
                ),
                hint: Some("`npm-run-all -p`, `concurrently`, `make -j`, or `cmd1 & cmd2 & wait` in a shell script.".into()),
                spans: chain,
                impact_ms: saving,
            });
        }
    }

    // 3. Many short-lived processes of the same tool.
    for g in &a.groups {
        let avg = g.wall_ms / g.count as f64;
        if g.count >= 50 && avg < 300.0 && g.self_ms >= (wall * 0.1).max(1000.0) {
            out.push(Insight {
                level: Level::Warn,
                text: format!(
                    "{} was started {} times ({} in total, {} each on average).",
                    code(&g.key),
                    g.count,
                    fmt_ms(g.self_ms),
                    fmt_ms(avg)
                ),
                hint: Some("Process startup adds up; batching the work into fewer invocations usually helps.".into()),
                spans: vec![g.top],
                impact_ms: g.self_ms,
            });
        }
    }

    // 4. The exact same command, several times.
    let mut same: HashMap<String, Vec<u32>> = HashMap::new();
    for s in spans.iter().filter(|s| !s.forked && s.parent.is_some()) {
        same.entry(format!("{}\0{}", s.cwd.as_deref().unwrap_or(""), s.argv.join("\0"))).or_default().push(s.id);
    }
    let mut repeated: Vec<(f64, Vec<u32>)> = same
        .into_values()
        .filter(|ids| ids.len() >= 2 && ids.len() < 50)
        .map(|ids| (ids.iter().map(|&i| spans[i as usize].dur()).sum::<f64>(), ids))
        .filter(|(total, ids)| {
            *total >= (wall * 0.05).max(1000.0) && ids.iter().all(|&i| spans[i as usize].dur() >= 200.0)
        })
        .collect();
    repeated.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (total, ids) in repeated.into_iter().take(2) {
        let s = &spans[ids[0] as usize];
        out.push(Insight {
            level: Level::Warn,
            text: format!(
                "{} ran {} times with the same arguments ({} in total).",
                code(&truncate(&shell_join(&s.argv[..s.argv.len().min(6)]), 60)),
                ids.len(),
                fmt_ms(total)
            ),
            hint: Some(
                "Check for a step that runs both from a hook (pre/post scripts) and from the main script.".into(),
            ),
            impact_ms: total * (ids.len() - 1) as f64 / ids.len() as f64,
            spans: ids,
        });
    }

    // 5. Long waits: processes (with everything under them) using no CPU.
    let mut waits: Vec<(f64, u32)> = spans
        .iter()
        .filter(|s| !covered.contains(&s.id) && !s.outlived)
        .map(|s| (idle_total(a, s.id), s.id))
        .filter(|(idle, _)| *idle >= (wall * 0.1).max(2000.0))
        .collect();
    waits.sort_by(|a, b| b.0.total_cmp(&a.0));
    // Keep the deepest process of a waiting chain (npm → sh → curl: report curl).
    let mut picked: Vec<(f64, u32)> = Vec::new();
    for (idle, id) in waits {
        let deeper_child = a.children[id as usize].iter().any(|&c| idle_total(a, c) >= idle * 0.8);
        if !deeper_child && picked.len() < 2 {
            picked.push((idle, id));
        }
    }
    for (idle, id) in picked {
        let s = &spans[id as usize];
        let text = if a.keys[id as usize] == "sleep" {
            format!("{} slept for {}.", code(lbl(id)), fmt_ms(idle))
        } else {
            format!(
                "{} sat idle for {} (CPU {:.0}% overall): waiting on the network, disk or another process.",
                code(lbl(id)),
                fmt_ms(idle),
                avg_cores(s) * 100.0
            )
        };
        covered.insert(id);
        out.push(Insight {
            level: Level::Warn,
            text,
            hint: tool_hint(&a.keys[id as usize], lbl(id), s),
            spans: vec![id],
            impact_ms: idle,
        });
    }

    // 6. Concrete flags for big steps that weren't mentioned yet.
    for g in a.groups.iter().filter(|g| g.self_ms >= (wall * 0.1).max(1000.0)) {
        if covered.contains(&g.top) {
            continue;
        }
        if let Some(hint) = tool_hint(&g.key, lbl(g.top), &spans[g.top as usize]) {
            out.push(Insight {
                level: Level::Tip,
                text: format!("{} took {} ({:.0}%).", code(lbl(g.top)), fmt_ms(g.self_ms), g.self_ms / wall * 100.0),
                hint: Some(hint),
                spans: vec![g.top],
                impact_ms: g.self_ms * 0.5,
            });
        }
    }

    // 7. Long silence in the output.
    if !t.output.is_empty() {
        let mut marks: Vec<f64> = vec![0.0];
        marks.extend(t.output.iter().map(|l| l.t_ms));
        marks.push(wall);
        let (mut gap, mut at) = (0.0, 0usize);
        for (i, w) in marks.windows(2).enumerate() {
            if w[1] - w[0] > gap {
                gap = w[1] - w[0];
                at = i;
            }
        }
        if gap >= (wall * 0.25).max(5000.0) {
            let (g0, g1) = (marks[at], marks[at + 1]);
            // Who was busy: most overlap, preferring the deepest process.
            let overlap = |s: &Span| (s.end_ms.min(g1) - s.start_ms.max(g0)).max(0.0);
            let best = spans.iter().map(overlap).fold(0.0, f64::max);
            let who =
                spans.iter().filter(|s| overlap(s) >= best * 0.8 && best > 0.0).max_by_key(|s| a.depth[s.id as usize]);
            let mut text = format!("No output for {} (from {} to {})", fmt_ms(gap), fmt_ms(g0), fmt_ms(g1));
            if let Some(w) = who {
                text.push_str(&format!(" while {} was running", code(lbl(w.id))));
            }
            text.push('.');
            let hint = (at > 0).then(|| format!("Last line before it: \"{}\"", truncate(&t.output[at - 1].text, 80)));
            out.push(Insight {
                level: Level::Info,
                text,
                hint,
                spans: who.map(|w| vec![w.id]).unwrap_or_default(),
                impact_ms: 0.0,
            });
        }
    }

    // 8. Memory.
    if let Some(s) = spans.iter().max_by_key(|s| s.max_rss_kb).filter(|s| s.max_rss_kb >= 1536 * 1024) {
        out.push(Insight {
            level: Level::Info,
            text: format!("{} peaked at {} of memory.", code(lbl(s.id)), fmt_kb(s.max_rss_kb)),
            hint: None,
            spans: vec![s.id],
            impact_ms: 0.0,
        });
    }

    // 9. Processes still running after the command.
    let outlived: Vec<u32> = spans.iter().filter(|s| s.outlived).map(|s| s.id).collect();
    if !outlived.is_empty() {
        let mut names: Vec<String> = outlived.iter().map(|&i| code(&a.keys[i as usize])).collect();
        names.dedup();
        out.push(Insight {
            level: Level::Info,
            text: format!(
                "{} process{} kept running after the command exited ({}); not measured past that point.",
                outlived.len(),
                if outlived.len() > 1 { "es" } else { "" },
                names.into_iter().take(3).collect::<Vec<_>>().join(", ")
            ),
            hint: None,
            spans: outlived,
            impact_ms: 0.0,
        });
    }

    let rank = |l: Level| match l {
        Level::Warn => 0,
        Level::Tip => 1,
        Level::Info => 2,
    };
    out.sort_by(|a, b| rank(a.level).cmp(&rank(b.level)).then(b.impact_ms.total_cmp(&a.impact_ms)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: u32, parent: Option<u32>, argv: &str, start: f64, end: f64) -> Span {
        let mut s = Span::new(id, parent, 100 + id as i32, argv.split(' ').map(String::from).collect(), start);
        s.end_ms = end;
        s
    }

    fn trace(spans: Vec<Span>) -> Trace {
        Trace {
            version: 1,
            tool_version: "test".into(),
            command: spans[0].argv.clone(),
            cwd: "/".into(),
            os: "linux".into(),
            cpus: 8,
            backend: "test".into(),
            started_unix_ms: 0,
            wall_ms: spans[0].dur(),
            exit_code: Some(0),
            signal: None,
            interrupted: false,
            sample_ms: 100,
            ticks: vec![],
            spans,
            output: vec![],
            output_dropped: 0,
        }
    }

    #[test]
    fn attribution_sums_to_wall_and_splits_parallel_leaves() {
        let t = trace(vec![
            span(0, None, "npm run build", 0.0, 10_000.0),
            span(1, Some(0), "sh -c a", 100.0, 9_900.0),
            span(2, Some(1), "vite build", 200.0, 4_200.0),
            span(3, Some(1), "tsc", 4_200.0, 9_800.0),
            span(4, Some(3), "esbuild", 5_000.0, 6_000.0),
        ]);
        let a = analyze(&t);
        let total: f64 = a.self_ms.iter().sum();
        assert!((total - 10_000.0).abs() < 1e-6);
        assert!((a.self_ms[2] - 4_000.0).abs() < 1e-6);
        assert!((a.self_ms[3] - 4_600.0).abs() < 1e-6);
        assert!((a.self_ms[4] - 1_000.0).abs() < 1e-6);
        assert_eq!(a.rows, vec![0, 1, 2, 2, 3]);
        // vite → tsc ran sequentially under the shell.
        assert!(a.insights.iter().any(|i| i.text.contains("one after another")));
    }

    #[test]
    fn parallel_children_get_their_own_rows() {
        let t = trace(vec![
            span(0, None, "make -j2", 0.0, 1_000.0),
            span(1, Some(0), "cc a.c", 0.0, 900.0),
            span(2, Some(0), "cc b.c", 10.0, 950.0),
        ]);
        let a = analyze(&t);
        assert_eq!(a.rows, vec![0, 1, 2]);
        assert!((a.self_ms[1] - 455.0).abs() < 1e-6);
    }
}
