//! Project tasks that need more than one cargo command. Run them with `cargo xtask <task>`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

type Result<T = ()> = std::result::Result<T, String>;

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  web    build the web board into crates/rodu-web/dist/";

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("web") => web(),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown task `{other}`\n\n{USAGE}")),
        None => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives in the workspace").into()
}

/// Run `command`, failing with its name when it cannot start or exits unsuccessfully.
fn run(command: &mut Command) -> Result {
    let name = command.get_program().to_string_lossy().into_owned();
    let status = command.status().map_err(|e| format!("could not run `{name}`: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("`{name}` failed ({status})")) }
}

/// Output of `program args` on stdout, or `None` when it is not installed or fails.
fn probe(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The version of `package` that Cargo.lock pins.
fn locked_version(lock: &str, package: &str) -> Option<String> {
    let name_line = format!("name = \"{package}\"");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() == name_line {
            let version = lines.next()?.trim().strip_prefix("version = \"")?.strip_suffix('"')?;
            return Some(version.to_string());
        }
    }
    None
}

/// The wasm-bindgen CLI must match the `wasm-bindgen` crate exactly, or its output will not load.
fn check_wasm_bindgen(root: &Path) -> Result {
    let lock = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|e| format!("could not read Cargo.lock: {e}"))?;
    let wanted = locked_version(&lock, "wasm-bindgen")
        .ok_or("Cargo.lock has no `wasm-bindgen` package; is rodu-web in the workspace?")?;
    let install = format!("cargo install wasm-bindgen-cli --version {wanted} --locked");
    match probe("wasm-bindgen", &["--version"]) {
        None => Err(format!("`wasm-bindgen` was not found on PATH. Install it with:\n  {install}")),
        Some(found) if found.split_whitespace().nth(1) == Some(wanted.as_str()) => Ok(()),
        Some(found) => Err(format!(
            "`{found}` does not match the wasm-bindgen {wanted} crate in Cargo.lock. Install the \
             matching CLI with:\n  {install}"
        )),
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result {
    let entries =
        fs::read_dir(from).map_err(|e| format!("could not read {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .map_err(|e| format!("could not copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// `cargo xtask web`: compile the board to WebAssembly, generate its JS glue and gather the
/// static files into `crates/rodu-web/dist/`, which `rodu web` serves.
fn web() -> Result {
    let root = workspace_root();
    check_wasm_bindgen(&root)?;

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    run(Command::new(cargo).current_dir(&root).args([
        "build",
        "--package",
        "rodu-web",
        "--bin",
        "rodu-web",
        "--target",
        "wasm32-unknown-unknown",
        "--profile",
        "wasm-release",
    ]))?;

    let crate_dir = root.join("crates/rodu-web");
    let dist = crate_dir.join("dist");
    if dist.exists() {
        fs::remove_dir_all(&dist)
            .map_err(|e| format!("could not clear {}: {e}", dist.display()))?;
    }
    fs::create_dir_all(&dist).map_err(|e| format!("could not create {}: {e}", dist.display()))?;

    let wasm = root.join("target/wasm32-unknown-unknown/wasm-release/rodu-web.wasm");
    run(Command::new("wasm-bindgen")
        .args(["--target", "web", "--no-typescript", "--out-name", "rodu_web", "--out-dir"])
        .arg(&dist)
        .arg(&wasm))?;

    let output = dist.join("rodu_web_bg.wasm");
    if probe("wasm-opt", &["--version"]).is_some() {
        // The features rustc enables by default for wasm32-unknown-unknown.
        run(Command::new("wasm-opt")
            .args([
                "-Oz",
                "--enable-bulk-memory",
                "--enable-mutable-globals",
                "--enable-nontrapping-float-to-int",
                "--enable-sign-ext",
                "--enable-reference-types",
                "--enable-multivalue",
            ])
            .arg(&output)
            .arg("-o")
            .arg(&output))?;
    } else {
        println!("note: wasm-opt not found; skipping the extra size pass (install binaryen)");
    }

    copy_dir(&crate_dir.join("static"), &dist)?;

    let size = fs::metadata(&output).map_err(|e| e.to_string())?.len();
    println!("built {} ({} KiB wasm)", dist.display(), size.div_ceil(1024));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::locked_version;

    #[test]
    fn locked_version_reads_the_exact_package() {
        let lock = "[[package]]\nname = \"wasm-bindgen-futures\"\nversion = \"0.4.79\"\n\n\
                    [[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.129\"\n";
        assert_eq!(locked_version(lock, "wasm-bindgen").as_deref(), Some("0.2.129"));
        assert_eq!(locked_version(lock, "leptos"), None);
    }
}
