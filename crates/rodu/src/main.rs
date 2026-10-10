use std::collections::HashMap;

use rodu::team::CHECK_COMMAND;
use rodu::{Io, open_url, run};

fn main() {
    let argv: Vec<String> = match std::env::args_os().skip(1).map(|a| a.into_string()).collect() {
        Ok(argv) => argv,
        Err(arg) => {
            eprintln!("error: argument {arg:?} is not valid Unicode");
            std::process::exit(1);
        }
    };
    // The child process that replays a sync import, so a crash in the decoder stays here.
    if argv == [CHECK_COMMAND] {
        let code = match rodu_sync::run_check(&mut std::io::stdin().lock()) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                e.exit_code()
            }
        };
        std::process::exit(code);
    }
    // Variables that are not valid Unicode cannot name a workspace; skip them, never panic.
    let env: HashMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("error: cannot read the current folder: {e}");
            std::process::exit(1);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start: {e}");
            std::process::exit(1);
        }
    };
    let mut out = |line: &str| println!("{line}");
    let mut err = |line: &str| eprintln!("{line}");
    let mut open = |url: &str| open_url(url);
    let mut io = Io {
        cwd,
        env,
        out: &mut out,
        err: &mut err,
        open_url: Some(&mut open),
        on_web_server: None,
    };
    let code = runtime.block_on(run(&argv, &mut io));
    std::process::exit(code);
}
