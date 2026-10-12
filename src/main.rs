#![allow(dead_code)]
mod capture;
mod label;
mod model;
#[cfg(unix)]
mod poll_backend;
#[cfg(target_os = "linux")]
mod ptrace_backend;
mod record;
mod signals;
mod util;

fn main() {}
