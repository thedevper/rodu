//! Project tasks that need more than one cargo command. Run them with `cargo xtask <task>`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

type Result<T = ()> = std::result::Result<T, String>;

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  web                  build the web board into crates/rodu-web/dist/
  notices              write dist/THIRD-PARTY-NOTICES.txt; fails on a licence not in about.toml
  dist [--target T]    build a release archive into dist/release/ (T: darwin-arm64, darwin-x64,
                       windows-x64, linux-x64; default: this machine)
  release-files        write SHA256SUMS, the Homebrew formula and the Scoop manifest for the
                       archives in dist/release/, and copy the install scripts beside them
  smoke [BINARY]       drive a built rodu like a new user: init, add, ls, web and mcp
                       (default: this machine's release build)";

const REPO: &str = "TheDevper/rodu";
const DESCRIPTION: &str = "Local-first, AI-first kanban for small teams";
/// Shipped next to the binary in every archive.
const NOTICES: [&str; 3] = ["LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.txt"];

#[derive(Clone, Copy)]
struct Target {
    /// The name in archive file names and the install scripts.
    name: &'static str,
    triple: &'static str,
    zip: bool,
}

const TARGETS: [Target; 4] = [
    Target { name: "darwin-arm64", triple: "aarch64-apple-darwin", zip: false },
    Target { name: "darwin-x64", triple: "x86_64-apple-darwin", zip: false },
    Target { name: "windows-x64", triple: "x86_64-pc-windows-msvc", zip: true },
    Target { name: "linux-x64", triple: "x86_64-unknown-linux-gnu", zip: false },
];

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("web") => web(),
        Some("notices") => notices().map(|_| ()),
        Some("dist") => dist(std::env::args().skip(2).collect()),
        Some("release-files") => release_files(),
        Some("smoke") => smoke(std::env::args().nth(2)),
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

/// Cargo's output folder, honouring `CARGO_TARGET_DIR`.
fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => root.join(dir),
        _ => root.join("target"),
    }
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
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

    run(cargo().current_dir(&root).args([
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

    let wasm = target_dir(&root).join("wasm32-unknown-unknown/wasm-release/rodu-web.wasm");
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

/// The release version: `[workspace.package] version` in the root Cargo.toml.
fn workspace_version(manifest: &str) -> Option<String> {
    let section = manifest.split("[workspace.package]").nth(1)?;
    let section = section.split("\n[").next()?;
    section.lines().find_map(|line| {
        let value = line.trim().strip_prefix("version")?.trim_start().strip_prefix('=')?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

fn version(root: &Path) -> Result<String> {
    let manifest = fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    workspace_version(&manifest).ok_or_else(|| "no version in [workspace.package]".into())
}

/// `cargo xtask notices`: the licences of every crate shipped in the binary and the board.
fn notices() -> Result<PathBuf> {
    let root = workspace_root();
    if probe("cargo-about", &["--version"]).is_none() {
        return Err("cargo-about was not found. Install it with:\n  \
                    cargo install cargo-about --locked --features cli"
            .into());
    }
    let out_dir = root.join("dist");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let out = out_dir.join("THIRD-PARTY-NOTICES.txt");
    run(cargo()
        .current_dir(&root)
        .args(["about", "generate", "--workspace", "--locked", "--fail"])
        .arg("about.hbs")
        .arg("--output-file")
        .arg(&out))?;
    println!("wrote {}", out.display());
    Ok(out)
}

fn host_target() -> Option<Target> {
    let name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("windows", "x86_64") => "windows-x64",
        ("linux", "x86_64") => "linux-x64",
        _ => return None,
    };
    TARGETS.into_iter().find(|t| t.name == name)
}

fn archive_name(version: &str, target: Target) -> String {
    let ext = if target.zip { "zip" } else { "tar.gz" };
    format!("rodu-v{version}-{}.{ext}", target.name)
}

/// `cargo xtask dist`: the board, the notices and a release build, packed for one target.
fn dist(args: Vec<String>) -> Result {
    let wanted = match args.as_slice() {
        [] => None,
        [flag, name] if flag == "--target" => Some(name.as_str()),
        _ => return Err(format!("unexpected arguments: {}\n\n{USAGE}", args.join(" "))),
    };
    let target = match wanted {
        Some(name) => TARGETS.into_iter().find(|t| t.name == name).ok_or_else(|| {
            let names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
            format!("unknown target `{name}`; use one of {}", names.join(", "))
        })?,
        None => host_target().ok_or("no release target for this machine; pass --target")?,
    };
    let root = workspace_root();
    let version = version(&root)?;

    // The board first: build.rs embeds whatever crates/rodu-web/dist holds when rodu compiles.
    web()?;
    let third_party = notices()?;
    run(cargo()
        .current_dir(&root)
        .args(["build", "--release", "--locked", "--package", "rodu"])
        .args(["--target", target.triple]))?;

    let exe = if target.zip { "rodu.exe" } else { "rodu" };
    let binary = target_dir(&root).join(target.triple).join("release").join(exe);
    if host_target().is_some_and(|h| h.triple == target.triple) {
        let reported = probe(&binary.to_string_lossy(), &["--version"]).unwrap_or_default();
        if reported != format!("rodu {version}") {
            return Err(format!("{} --version printed `{reported}`", binary.display()));
        }
    }

    let stage = target_dir(&root).join("xtask-dist").join(target.name);
    if stage.exists() {
        fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let copy = |from: &Path, name: &str| {
        fs::copy(from, stage.join(name))
            .map(|_| ())
            .map_err(|e| format!("could not copy {}: {e}", from.display()))
    };
    copy(&binary, exe)?;
    copy(&root.join("LICENSE"), "LICENSE")?;
    copy(&root.join("NOTICE"), "NOTICE")?;
    copy(&third_party, "THIRD-PARTY-NOTICES.txt")?;

    let release = root.join("dist/release");
    fs::create_dir_all(&release).map_err(|e| e.to_string())?;
    let archive = release.join(archive_name(&version, target));
    let _ = fs::remove_file(&archive);
    // bsdtar (macOS, Windows 10+) writes a zip when the name ends in .zip.
    let flags = if target.zip { "-a -cf" } else { "-czf" };
    run(Command::new("tar")
        .args(flags.split(' '))
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .arg(exe)
        .args(NOTICES))?;
    println!("built {}", archive.display());
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// `cargo xtask release-files`: what installers need beside the archives of one release.
fn release_files() -> Result {
    let root = workspace_root();
    let version = version(&root)?;
    let release = root.join("dist/release");
    let mut sums = Vec::new();
    for target in TARGETS {
        let name = archive_name(&version, target);
        let path = release.join(&name);
        if path.is_file() {
            sums.push((target, name, sha256_file(&path)?));
        }
    }
    if sums.is_empty() {
        return Err(format!("no rodu-v{version}-* archives in {}", release.display()));
    }
    let lines: Vec<String> = sums.iter().map(|(_, name, sum)| format!("{sum}  {name}")).collect();
    let write = |name: &str, text: String| {
        fs::write(release.join(name), text).map_err(|e| format!("could not write {name}: {e}"))
    };
    write("SHA256SUMS", format!("{}\n", lines.join("\n")))?;
    let sum = |name: &str| sums.iter().find(|(t, ..)| t.name == name).map(|(.., s)| s.as_str());
    if sum("darwin-arm64").is_some() || sum("darwin-x64").is_some() {
        write("rodu.rb", formula(&version, sum("darwin-arm64"), sum("darwin-x64")))?;
    }
    if let Some(windows) = sum("windows-x64") {
        write("rodu.json", scoop(&version, windows))?;
    }
    for script in ["install.sh", "install.ps1"] {
        fs::copy(root.join("packaging").join(script), release.join(script))
            .map_err(|e| format!("could not copy {script}: {e}"))?;
    }
    println!("{}", lines.join("\n"));
    Ok(())
}

fn download_url(version: &str, target: &str, ext: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/v{version}/rodu-v{version}-{target}.{ext}")
}

fn formula(version: &str, arm: Option<&str>, intel: Option<&str>) -> String {
    let arch = |target: &str, sum: Option<&str>| match sum {
        Some(sum) => format!(
            "      url \"{}\"\n      sha256 \"{sum}\"\n",
            download_url(version, target, "tar.gz")
        ),
        None => String::new(),
    };
    let notices: Vec<String> = NOTICES.iter().map(|n| format!("\"{n}\"")).collect();
    format!(
        r##"class Rodu < Formula
  desc "{DESCRIPTION}"
  homepage "https://github.com/{REPO}"
  version "{version}"
  license "Apache-2.0"

  on_macos do
    on_arm do
{arm}    end
    on_intel do
{intel}    end
  end

  def install
    bin.install "rodu"
    prefix.install {notices}
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/rodu --version")
  end
end
"##,
        arm = arch("darwin-arm64", arm),
        intel = arch("darwin-x64", intel),
        notices = notices.join(", "),
    )
}

fn scoop(version: &str, hash: &str) -> String {
    let manifest = serde_json::json!({
        "version": version,
        "description": DESCRIPTION,
        "homepage": format!("https://github.com/{REPO}"),
        "license": "Apache-2.0",
        "architecture": { "64bit": { "url": download_url(version, "windows-x64", "zip"), "hash": hash } },
        "bin": "rodu.exe",
        "checkver": "github",
        "autoupdate": { "architecture": { "64bit": { "url": download_url("$version", "windows-x64", "zip") } } },
    });
    format!("{}\n", serde_json::to_string_pretty(&manifest).expect("a JSON value serializes"))
}

/// A GET over plain HTTP/1.1, as the board's own browser would send it: (status, content type, body).
fn http_get(base: &str, path: &str, bearer: Option<&str>) -> Result<(u16, String, String)> {
    use std::io::{Read, Write};
    let host = base.trim_start_matches("http://").trim_end_matches('/');
    let mut stream =
        std::net::TcpStream::connect(host).map_err(|e| format!("connect {host}: {e}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let auth = bearer.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("read {path}: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or(format!("{path}: no response"))?;
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let content_type = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_string())
        })
        .unwrap_or_default();
    Ok((status, content_type, body.to_string()))
}

/// Reads `child`'s stdout lines until one contains `marker`, failing after 15 seconds.
fn read_until(child: &mut std::process::Child, marker: &str) -> Result<String> {
    use std::io::BufRead;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.contains(marker) => return Ok(line),
            Ok(_) => {}
            Err(_) => return Err(format!("timed out waiting for `{marker}`")),
        }
    }
}

/// `cargo xtask smoke`: the checks a new user's first minutes would make, on a real binary.
fn smoke(binary: Option<String>) -> Result {
    use std::io::Write;
    use std::process::Stdio;
    let root = workspace_root();
    let binary = match binary {
        Some(path) => PathBuf::from(path),
        None => {
            let host = host_target().ok_or("no release target for this machine")?;
            let exe = if host.zip { "rodu.exe" } else { "rodu" };
            target_dir(&root).join(host.triple).join("release").join(exe)
        }
    };
    let binary = fs::canonicalize(&binary)
        .map_err(|e| format!("{}: {e} (run cargo xtask dist first)", binary.display()))?;
    let ws = std::env::temp_dir().join(format!("rodu-smoke-{}", std::process::id()));
    fs::create_dir_all(&ws).map_err(|e| e.to_string())?;
    let rodu = |args: &[&str]| -> Result<String> {
        let out = Command::new(&binary)
            .args(args)
            .current_dir(&ws)
            .env_remove("RODU_DIR")
            .env_remove("RODU_WEB_DIST")
            .output()
            .map_err(|e| format!("could not run {}: {e}", binary.display()))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            Ok(text)
        } else {
            Err(format!("rodu {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr)))
        }
    };
    let mut failures = Vec::new();
    let mut check = |name: &str, ok: bool, detail: &str| {
        println!(
            "{} {name}{}",
            if ok { "ok  " } else { "FAIL" },
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        );
        if !ok {
            failures.push(name.to_string());
        }
    };

    let result = (|| -> Result {
        let version = rodu(&["--version"])?;
        check("--version", version.trim().starts_with("rodu "), version.trim());
        rodu(&["init", "--name", "smoke", "--key", "SMK", "--title", "Smoke test"])?;
        rodu(&["add", "First", "card", "--assignee", "me"])?;
        let list = rodu(&["ls"])?;
        check("init, add, ls", list.contains("SMK-1 [Backlog] First card"), list.trim());

        let mut web = Command::new(&binary)
            .args(["web", "--port", "0", "--no-open"])
            .current_dir(&ws)
            .env_remove("RODU_DIR")
            .env_remove("RODU_WEB_DIST")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let served = (|| -> Result {
            let line = read_until(&mut web, "Open: ")?;
            let link = line.trim().trim_start_matches("Open: ");
            let (base, fragment) = link.split_once('#').ok_or("no fragment in the link")?;
            let bearer = fragment.split_once('=').map_or("", |(_, v)| v);
            let (status, _, page) = http_get(base, "/", None)?;
            check(
                "web serves the embedded board",
                status == 200 && page.contains("/boot.js"),
                base,
            );
            let (status, kind, _) = http_get(base, "/rodu_web_bg.wasm", None)?;
            check(
                "web serves the board's wasm",
                status == 200 && kind == "application/wasm",
                &kind,
            );
            let (status, _, me) = http_get(base, "/api/me", Some(bearer))?;
            check(
                "web API answers with the token",
                status == 200 && me.contains("smoke"),
                me.trim(),
            );
            let (status, ..) = http_get(base, "/api/me", None)?;
            check("web API refuses a missing token", status == 401, &status.to_string());
            Ok(())
        })();
        let _ = web.kill();
        let _ = web.wait();
        served?;

        let mut mcp = Command::new(&binary)
            .arg("mcp")
            .current_dir(&ws)
            .env_remove("RODU_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#;
        let answered = (|| -> Result {
            let mut stdin = mcp.stdin.take().ok_or("no stdin")?;
            writeln!(stdin, "{initialize}").map_err(|e| e.to_string())?;
            let line = read_until(&mut mcp, "\"id\":1")?;
            check(
                "mcp initialize",
                line.contains("serverInfo"),
                &line.chars().take(80).collect::<String>(),
            );
            Ok(())
        })();
        let _ = mcp.kill();
        let _ = mcp.wait();
        answered
    })();
    // Windows keeps a folder busy for a moment after its last process exits.
    for _ in 0..10 {
        if fs::remove_dir_all(&ws).is_ok() || !ws.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    result?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("smoke checks failed: {}", failures.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_workspace_version() {
        let manifest = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.2.3\"\n\
                        edition = \"2024\"\n\n[profile.release]\nversion = \"9\"\n";
        assert_eq!(workspace_version(manifest).as_deref(), Some("1.2.3"));
        assert_eq!(workspace_version("[package]\nversion = \"1\"\n"), None);
    }

    #[test]
    fn names_archives_like_the_install_scripts_expect() {
        let [arm, _, windows, _] = TARGETS;
        assert_eq!(archive_name("0.3.0", arm), "rodu-v0.3.0-darwin-arm64.tar.gz");
        assert_eq!(archive_name("0.3.0", windows), "rodu-v0.3.0-windows-x64.zip");
    }

    #[test]
    fn writes_installer_manifests_for_the_release() {
        let rb = formula("0.3.0", Some("aa"), None);
        assert!(rb.contains("releases/download/v0.3.0/rodu-v0.3.0-darwin-arm64.tar.gz"));
        assert!(rb.contains("sha256 \"aa\""));
        assert!(rb.contains("prefix.install \"LICENSE\", \"NOTICE\", \"THIRD-PARTY-NOTICES.txt\""));
        assert!(rb.contains("#{bin}/rodu --version"));
        let json: serde_json::Value = serde_json::from_str(&scoop("0.3.0", "bb")).unwrap();
        assert_eq!(json["architecture"]["64bit"]["hash"], "bb");
        assert_eq!(json["bin"], "rodu.exe");
    }

    #[test]
    fn locked_version_reads_the_exact_package() {
        let lock = "[[package]]\nname = \"wasm-bindgen-futures\"\nversion = \"0.4.79\"\n\n\
                    [[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.129\"\n";
        assert_eq!(locked_version(lock, "wasm-bindgen").as_deref(), Some("0.2.129"));
        assert_eq!(locked_version(lock, "leptos"), None);
    }
}
