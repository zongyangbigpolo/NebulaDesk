//! First-party detached local Agent service. No credentials are accepted here.

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let flag = args.next();
    let state = args.next();
    if flag.as_deref() != Some(std::ffi::OsStr::new("--state"))
        || state.as_ref().is_none_or(|value| value.is_empty())
        || args.next().is_some()
    {
        eprintln!("Usage: nebula-desktop-agent --state <identity-path>");
        std::process::exit(2);
    }
    if nebula_desktop::local_host::daemon::run_process(PathBuf::from(state.unwrap())).is_err() {
        eprintln!("The local Agent service could not run safely.");
        std::process::exit(1);
    }
}
