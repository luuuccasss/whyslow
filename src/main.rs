mod analyze;
mod capture;
mod compare;
mod export;
mod label;
mod model;
#[cfg(unix)]
mod poll_backend;
#[cfg(target_os = "linux")]
mod ptrace_backend;
mod record;
mod render_term;
mod signals;
mod util;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "whyslow",
    version,
    about = "Run any command and see where the time went.",
    after_help = "Examples:\n  whyslow npm run build\n  whyslow --html make -j8\n  whyslow --json=before.json cargo build\n  whyslow compare before.json after.json"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command and explain where its time went (the default)
    Run(RunArgs),
    /// Print the report of a saved recording (--json)
    Show(ShowArgs),
    /// Compare two saved recordings
    Compare {
        before: PathBuf,
        after: PathBuf,
        #[arg(long)]
        no_color: bool,
    },
}

#[derive(Args)]
struct Outputs {
    /// Write an interactive HTML report [default: whyslow.html]
    #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "whyslow.html")]
    html: Option<PathBuf>,
    /// Save the raw recording, for `show` and `compare` [default: whyslow.json]
    #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "whyslow.json")]
    json: Option<PathBuf>,
    /// Write Chrome trace events, for ui.perfetto.dev [default: whyslow.trace.json]
    #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = true, default_missing_value = "whyslow.trace.json")]
    trace: Option<PathBuf>,
    /// Open the HTML report in the browser (implies --html)
    #[arg(long)]
    open: bool,
    /// Rows in the terminal timeline
    #[arg(long, default_value_t = 14, value_name = "N")]
    rows: usize,
    /// Don't print the report in the terminal
    #[arg(long)]
    no_report: bool,
    #[arg(long)]
    no_color: bool,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    out: Outputs,
    /// Don't show the command's own output
    #[arg(short, long)]
    quiet: bool,
    /// How processes are observed
    #[arg(long, value_enum, default_value = "auto")]
    backend: record::Backend,
    /// The command to run
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
    command: Vec<String>,
}

#[derive(Args)]
struct ShowArgs {
    file: PathBuf,
    #[command(flatten)]
    out: Outputs,
}

fn main() {
    // `whyslow npm run build` is short for `whyslow run npm run build`.
    let mut args: Vec<OsString> = std::env::args_os().collect();
    let first = args.get(1).and_then(|a| a.to_str()).unwrap_or("");
    if !first.is_empty() && !matches!(first, "run" | "show" | "compare" | "help" | "-h" | "--help" | "-V" | "--version")
    {
        args.insert(1, "run".into());
    }
    let cli = Cli::parse_from(args);
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("whyslow: {e:#}");
            std::process::exit(127);
        }
    }
}

fn color(no_color: bool) -> bool {
    !no_color && std::env::var_os("NO_COLOR").is_none() && util::isatty(2)
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Run(r) => {
            let trace = record::record(&r.command, r.backend, r.quiet)?;
            report(&trace, &r.out)?;
            Ok(match (trace.exit_code, trace.signal) {
                (Some(c), _) => c,
                (None, Some(s)) => 128 + s,
                _ => 1,
            })
        }
        Cmd::Show(s) => {
            let trace = model::Trace::load(&s.file)?;
            report(&trace, &s.out)?;
            Ok(0)
        }
        Cmd::Compare { before, after, no_color } => {
            let (bt, at) = (model::Trace::load(&before)?, model::Trace::load(&after)?);
            let (ba, aa) = (analyze::analyze(&bt), analyze::analyze(&at));
            let st = render_term::Style { color: color(no_color) };
            eprint!("{}", compare::render(&bt, &ba, &at, &aa, &st, util::term_width()));
            Ok(0)
        }
    }
}

fn report(trace: &model::Trace, out: &Outputs) -> Result<()> {
    let a = analyze::analyze(trace);
    let st = render_term::Style { color: color(out.no_color) };
    if !out.no_report {
        eprint!("{}", render_term::render(trace, &a, &st, util::term_width(), out.rows.max(1)));
    }
    let mut written = Vec::new();
    let html = out.html.clone().or_else(|| out.open.then(|| PathBuf::from("whyslow.html")));
    if let Some(p) = &html {
        export::write_html(trace, &a, p)?;
        written.push(format!("report   {}", p.display()));
    }
    if let Some(p) = &out.json {
        export::write_json(trace, p)?;
        written.push(format!("data     {}", p.display()));
    }
    if let Some(p) = &out.trace {
        export::write_chrome(trace, &a, p)?;
        written.push(format!("trace    {}  (open in ui.perfetto.dev)", p.display()));
    }
    if !out.no_report {
        if written.is_empty() {
            eprintln!("\n  {}", st.dim("Full interactive timeline: add --html (or --open)."));
        } else {
            eprintln!();
            for w in written {
                eprintln!("  {}", st.dim(&w));
            }
        }
        eprintln!();
    }
    if out.open {
        if let Some(p) = html {
            export::open_in_browser(&p);
        }
    }
    Ok(())
}
