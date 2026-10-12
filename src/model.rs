use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

/// A complete recording of one command run. This is what `--json` writes and
/// what `whyslow show` / `whyslow compare` read back.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Trace {
    pub version: u32,
    pub tool_version: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub os: String,
    pub cpus: u32,
    pub backend: String,
    pub started_unix_ms: u64,
    pub wall_ms: f64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub interrupted: bool,
    /// Sampling period of the CPU sampler.
    pub sample_ms: u32,
    /// End time (ms since start) of every CPU sample tick.
    pub ticks: Vec<f32>,
    /// One span per process, `spans[i].id == i`, parents always before children.
    pub spans: Vec<Span>,
    pub output: Vec<OutputLine>,
    #[serde(default)]
    pub output_dropped: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Span {
    pub id: u32,
    pub parent: Option<u32>,
    pub pid: i32,
    /// Command line of the last program this process executed.
    pub argv: Vec<String>,
    /// Earlier command lines when the process exec'd more than once (`sh -c` → `tsc`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prev_argv: Vec<Vec<String>>,
    /// The process forked but never exec'd: argv is inherited from the parent.
    #[serde(default)]
    pub forked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub start_ms: f64,
    pub end_ms: f64,
    pub user_ms: f64,
    pub sys_ms: f64,
    pub max_rss_kb: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Still running when the root command exited (daemons, background jobs).
    #[serde(default)]
    pub outlived: bool,
    /// Non-zero CPU samples as `[tick index, % of one core]`.
    #[serde(default)]
    pub samples: Vec<(u32, f32)>,
}

impl Span {
    pub fn new(id: u32, parent: Option<u32>, pid: i32, argv: Vec<String>, start_ms: f64) -> Span {
        Span {
            id,
            parent,
            pid,
            argv,
            prev_argv: Vec::new(),
            forked: false,
            cwd: None,
            start_ms,
            end_ms: start_ms,
            user_ms: 0.0,
            sys_ms: 0.0,
            max_rss_kb: 0,
            read_bytes: 0,
            write_bytes: 0,
            exit_code: None,
            signal: None,
            outlived: false,
            samples: Vec::new(),
        }
    }

    pub fn dur(&self) -> f64 {
        (self.end_ms - self.start_ms).max(0.0)
    }

    pub fn cpu_ms(&self) -> f64 {
        self.user_ms + self.sys_ms
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OutputLine {
    pub t_ms: f64,
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub err: bool,
}

impl Trace {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Trace> {
        use anyhow::Context;
        let data = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let t: Trace = serde_json::from_slice(&data)
            .with_context(|| format!("{} is not a whyslow recording (made with --json)", path.display()))?;
        if t.version > FORMAT_VERSION {
            anyhow::bail!("{} was written by a newer whyslow (format {})", path.display(), t.version);
        }
        for (i, s) in t.spans.iter().enumerate() {
            if s.id as usize != i || s.parent.is_some_and(|p| p as usize >= i) {
                anyhow::bail!("{}: corrupted span table", path.display());
            }
        }
        if t.spans.is_empty() {
            anyhow::bail!("{}: recording has no processes", path.display());
        }
        Ok(t)
    }
}
