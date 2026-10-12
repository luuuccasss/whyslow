//! Turns raw command lines into names people recognise:
//! `node /app/node_modules/typescript/bin/tsc -p .` → `tsc`,
//! `/usr/bin/node …/npm-cli.js run build` → `npm run build`,
//! `rustc --crate-name serde …` → `rustc serde`.

use crate::util::{basename, truncate};

pub struct Label {
    /// What is shown for this process.
    pub label: String,
    /// Tool name used to group processes ("tsc", "rustc", "npm").
    pub key: String,
}

const SUBCOMMAND_TOOLS: &[&str] = &[
    "git",
    "cargo",
    "go",
    "docker",
    "podman",
    "npm",
    "npx",
    "yarn",
    "pnpm",
    "bun",
    "deno",
    "make",
    "gmake",
    "gradle",
    "gradlew",
    "mvn",
    "dotnet",
    "kubectl",
    "pip",
    "pip3",
    "poetry",
    "uv",
    "terraform",
    "rustup",
    "composer",
    "bundle",
    "rake",
    "mix",
    "swift",
    "zig",
    "nx",
    "turbo",
    "vite",
    "next",
    "nuxt",
    "ng",
    "astro",
    "vitest",
    "pytest",
    "lerna",
    "corepack",
    "brew",
    "apt",
    "apt-get",
    "helm",
    "ninja",
    "cmake",
    "meson",
    "bazel",
];
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ash", "ksh", "fish"];
const WRAPPERS: &[&str] = &["env", "nice", "nohup", "time", "stdbuf", "ionice", "chrt", "setsid", "caffeinate"];
const COMPILERS: &[&str] =
    &["cc", "c++", "gcc", "g++", "clang", "clang++", "cc1", "cc1plus", "cc1obj", "as", "javac", "swiftc"];
const SOURCE_EXT: &[&str] = &[".c", ".cc", ".cpp", ".cxx", ".c++", ".m", ".mm", ".s", ".S", ".java", ".swift"];
/// Flags that take the following argument as their value.
const VALUE_FLAGS: &[&str] = &[
    "-C",
    "-f",
    "-o",
    "-p",
    "-c",
    "--project",
    "--config",
    "--filter",
    "-F",
    "-w",
    "--workspace",
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "--cwd",
    "--prefix",
    "-I",
    "-L",
    "-W",
    "-X",
    "--manifest-path",
    "-j",
    "--jobs",
    "--target",
    "-t",
    "-e",
    "--env",
    "--title",
    "--inspect-port",
    "--mode",
];
const GENERIC_STEMS: &[&str] = &["cli", "index", "main", "bin", "run", "start", "entry", "launcher", "app", "program"];

fn stem(path: &str) -> &str {
    let b = basename(path);
    for ext in [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".py", ".rb", ".pl", ".sh", ".jar", ".exe", ".php"] {
        if let Some(x) = b.strip_suffix(ext) {
            if !x.is_empty() {
                return x;
            }
        }
    }
    b
}

fn is_word(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic())
        && s.len() <= 28
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c))
}

/// Positional arguments after the program, skipping flags and their values.
fn positionals(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            out.extend(args[i + 1..].iter().map(|s| s.as_str()));
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            if VALUE_FLAGS.contains(&a) {
                i += 1;
            }
        } else {
            out.push(a);
        }
        i += 1;
    }
    out
}

/// Name of the tool behind a script path, using node_modules layout when present.
fn script_tool(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let last = parts.last().copied().unwrap_or(path);
    let st = stem(last);
    if let Some(i) = parts.iter().rposition(|p| *p == "node_modules") {
        let mut j = i + 1;
        if parts.get(j) == Some(&".bin") {
            return parts.get(j + 1).map(|p| stem(p).to_string()).unwrap_or_else(|| st.to_string());
        }
        if parts.get(j).is_some_and(|p| p.starts_with('@')) && j + 1 < parts.len() - 1 {
            j += 1;
        }
        if let Some(pkg) = parts.get(j).filter(|_| j < parts.len() - 1) {
            let lower = st.to_ascii_lowercase();
            let generic = GENERIC_STEMS.contains(&lower.as_str())
                || lower.contains("child")
                || lower.contains("worker")
                || lower.contains("process");
            let name = if generic { pkg } else { st };
            return name.strip_suffix("-cli").unwrap_or(name).to_string();
        }
    }
    st.strip_suffix("-cli").unwrap_or(st).to_string()
}

fn with_subcommand(tool: &str, rest: &[String]) -> String {
    let mut label = tool.to_string();
    if !SUBCOMMAND_TOOLS.contains(&tool) {
        return label;
    }
    let pos = positionals(rest);
    if let Some(first) = pos.first().filter(|p| is_word(p)) {
        label.push(' ');
        label.push_str(first);
        if matches!(*first, "run" | "run-script" | "exec" | "x" | "dlx" | "test" | "workspace") {
            if let Some(second) = pos.get(1).filter(|p| is_word(p)) {
                label.push(' ');
                label.push_str(second);
            }
        }
    }
    label
}

pub fn label(argv: &[String]) -> Label {
    label_depth(argv, 0)
}

fn label_depth(argv: &[String], depth: u8) -> Label {
    let Some(arg0) = argv.first() else {
        return Label { label: "?".into(), key: "?".into() };
    };
    let base = basename(arg0);
    let rest = &argv[1..];
    let simple = |l: String, k: &str| Label { label: l, key: k.to_string() };

    if WRAPPERS.contains(&base) && depth < 4 {
        // env FOO=1 nice -n 5 cmd … → label of cmd
        let mut i = 0;
        while i < rest.len() && (rest[i].starts_with('-') || rest[i].contains('=') || rest[i].parse::<f64>().is_ok()) {
            i += 1;
        }
        if i < rest.len() {
            return label_depth(&rest[i..], depth + 1);
        }
    }

    if SHELLS.contains(&base) {
        let c_at = rest.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'));
        if let Some(i) = c_at {
            if let Some(script) = rest[i + 1..].iter().find(|a| !a.starts_with('-')) {
                let one_line = script.split_whitespace().collect::<Vec<_>>().join(" ");
                return simple(format!("{base} -c {}", truncate(&one_line, 48)), base);
            }
        }
        if let Some(script) = positionals(rest).first() {
            return simple(format!("{base} {}", basename(script)), base);
        }
        return simple(base.to_string(), base);
    }

    // Interpreters: name the script, not the interpreter.
    let interp = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if matches!(interp, "node" | "nodejs" | "bun" | "deno" | "tsx" | "ts-node") {
        let mut i = 0;
        while i < rest.len() {
            let a = rest[i].as_str();
            if matches!(a, "-e" | "--eval" | "-p" | "--print") {
                return simple(format!("{base} -e"), base);
            }
            if base == "deno" && matches!(a, "run" | "task" | "test") {
                i += 1;
                continue;
            }
            if a.starts_with('-') {
                if VALUE_FLAGS.contains(&a) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            let tool = script_tool(a);
            return simple(with_subcommand(&tool, &rest[i + 1..]), &tool);
        }
        return simple(base.to_string(), base);
    }
    if matches!(interp, "python" | "pypy" | "ruby" | "perl" | "php") {
        let mut i = 0;
        while i < rest.len() {
            let a = rest[i].as_str();
            if a == "-m" {
                if let Some(m) = rest.get(i + 1) {
                    let tool = m.rsplit('.').next().unwrap_or(m).to_string();
                    return simple(with_subcommand(&tool, &rest[i + 2..]), &tool);
                }
            }
            if a == "-c" || a == "-e" {
                return simple(format!("{base} {a}"), base);
            }
            if a.starts_with('-') {
                if matches!(a, "-W" | "-X" | "-I") {
                    i += 1;
                }
                i += 1;
                continue;
            }
            let tool = script_tool(a);
            return simple(with_subcommand(&tool, &rest[i + 1..]), &tool);
        }
        return simple(base.to_string(), base);
    }
    if base == "java" {
        let mut i = 0;
        while i < rest.len() {
            let a = rest[i].as_str();
            if a == "-jar" {
                if let Some(j) = rest.get(i + 1) {
                    let tool = stem(j).to_string();
                    return simple(tool.clone(), &tool);
                }
            }
            if matches!(a, "-cp" | "-classpath" | "--class-path" | "-p" | "--module-path" | "-m" | "--module") {
                i += 2;
                continue;
            }
            if a.starts_with('-') {
                i += 1;
                continue;
            }
            let class = a.rsplit('.').next().unwrap_or(a);
            let tool = match class {
                "GradleMain" | "GradleWrapperMain" => "gradle",
                "GradleDaemon" => "gradle-daemon",
                "GradleWorkerMain" => "gradle-worker",
                "Launcher" if a.contains("plexus") => "mvn",
                "KotlinCompileDaemon" => "kotlin-daemon",
                _ => class,
            };
            return simple(tool.to_string(), tool);
        }
        return simple("java".into(), "java");
    }

    if base == "rustc" {
        if let Some(i) = rest.iter().position(|a| a == "--crate-name") {
            if let Some(c) = rest.get(i + 1) {
                return simple(format!("rustc {c}"), "rustc");
            }
        }
        return simple("rustc".into(), "rustc");
    }
    if COMPILERS.contains(&base)
        || base.starts_with("gcc-")
        || base.starts_with("clang-")
        || base.ends_with("-gcc")
        || base.ends_with("-g++")
    {
        if let Some(src) = rest.iter().rev().find(|a| !a.starts_with('-') && SOURCE_EXT.iter().any(|e| a.ends_with(e)))
        {
            return simple(format!("{base} {}", basename(src)), base);
        }
        if rest.iter().any(|a| a == "-o") && !rest.iter().any(|a| a == "-c" || a == "-S" || a == "-E") {
            return simple(format!("{base} (link)"), base);
        }
        return simple(base.to_string(), base);
    }

    let tool = base.strip_suffix(".exe").unwrap_or(base);
    let tool = if tool == "gradlew" { "gradle" } else { tool };
    if SUBCOMMAND_TOOLS.contains(&tool) {
        return simple(with_subcommand(tool, rest), tool);
    }
    simple(tool.to_string(), tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(s: &str) -> String {
        label(&s.split(' ').map(String::from).collect::<Vec<_>>()).label
    }

    #[test]
    fn names() {
        assert_eq!(l("node /app/node_modules/typescript/bin/tsc -p tsconfig.json"), "tsc");
        assert_eq!(l("/usr/bin/node /usr/lib/node_modules/npm/bin/npm-cli.js run build"), "npm run build");
        assert_eq!(l("node /app/node_modules/.bin/vite build"), "vite build");
        assert_eq!(l("node /p/node_modules/webpack-cli/bin/cli.js --mode production"), "webpack");
        assert_eq!(l("node /p/node_modules/jest-worker/build/workers/processChild.js"), "jest-worker");
        assert_eq!(l("/p/node_modules/@esbuild/linux-x64/bin/esbuild --service=0.21.5 --ping"), "esbuild");
        assert_eq!(l("sh -c tsc"), "sh -c tsc");
        assert_eq!(l("rustc --crate-name serde --edition=2018 src/lib.rs"), "rustc serde");
        assert_eq!(l("cc -O2 -c src/main.c -o main.o"), "cc main.c");
        assert_eq!(l("cc main.o util.o -o app"), "cc (link)");
        assert_eq!(l("env NODE_ENV=production node build.js"), "build");
        assert_eq!(l("python3 -m pytest -x tests"), "pytest tests");
        assert_eq!(l("make -j8 all"), "make all");
        assert_eq!(l("cargo build --release"), "cargo build");
        assert_eq!(l("java -cp x.jar org.gradle.launcher.daemon.bootstrap.GradleDaemon 8.5"), "gradle-daemon");
        assert_eq!(l("docker build -t app ."), "docker build");
    }
}
