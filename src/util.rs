use std::time::{SystemTime, UNIX_EPOCH};

pub fn fmt_ms(ms: f64) -> String {
    let ms = ms.max(0.0);
    if ms < 1000.0 {
        format!("{:.0}ms", ms)
    } else if ms < 10_000.0 {
        format!("{:.2}s", ms / 1000.0)
    } else if ms < 60_000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        let s = (ms / 1000.0).round() as u64;
        if s < 3600 {
            format!("{}m{:02}s", s / 60, s % 60)
        } else {
            format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
        }
    }
}

pub fn fmt_delta(ms: f64) -> String {
    if ms.abs() < 0.5 {
        return "±0ms".into();
    }
    format!("{}{}", if ms < 0.0 { "-" } else { "+" }, fmt_ms(ms.abs()))
}

pub fn fmt_kb(kb: u64) -> String {
    fmt_bytes(kb.saturating_mul(1024))
}

pub fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    const K: f64 = 1024.0;
    if b < K {
        format!("{} B", b)
    } else if b < K * K {
        format!("{:.0} KB", b / K)
    } else if b < K * K * K {
        format!("{:.0} MB", b / K / K)
    } else {
        format!("{:.1} GB", b / K / K / K)
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Cut to `max` characters, ending with an ellipsis when something was removed.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            if !c.is_control() || c == '\t' {
                out.push(c);
            }
            continue;
        }
        match it.next() {
            // CSI: parameters then one final byte in @..~
            Some('[') => {
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: until BEL or ST (ESC \)
            Some(']') => {
                while let Some(c) = it.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        it.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn basename(s: &str) -> &str {
    s.rsplit('/').find(|p| !p.is_empty()).unwrap_or(s)
}

pub fn term_width() -> usize {
    #[cfg(unix)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(100)
}

pub fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn cpu_count() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
}
