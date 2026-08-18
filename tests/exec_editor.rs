#![cfg(unix)]

use std::process::Command;
use std::process::Stdio;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;

#[test]
fn replaces_the_cli_process_with_the_editor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    let cache = directory.path().join("cache");
    let editor = directory.path().join("editor");
    std::fs::create_dir(&workspace)?;
    let status = Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(&workspace)
        .status()
        .context("initialize test Git repository")?;
    ensure!(status.success(), "git init failed with {status}");
    std::fs::write(workspace.join("sample.rs"), "pub struct Target;\n")?;
    let editor_source = directory.path().join("editor.rs");
    std::fs::write(
        &editor_source,
        "fn main() { println!(\"{}\", std::process::id()); }\n",
    )?;
    let status = Command::new("rustc")
        .arg(&editor_source)
        .arg("-o")
        .arg(&editor)
        .status()
        .context("compile test editor")?;
    ensure!(status.success(), "rustc failed with {status}");

    let child = Command::new(env!("CARGO_BIN_EXE_s"))
        .arg("Target")
        .arg(&workspace)
        .arg("--nvim")
        .arg(&editor)
        .env("XDG_CACHE_HOME", cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start s")?;
    let cli_pid = child.id();
    let output = child.wait_with_output()?;

    ensure!(
        output.status.success(),
        "s failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let editor_pid = String::from_utf8(output.stdout)?
        .trim()
        .parse::<u32>()
        .context("editor did not print its process ID")?;
    assert_eq!(editor_pid, cli_pid);
    Ok(())
}
