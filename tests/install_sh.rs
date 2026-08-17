#![cfg(unix)]

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;

#[test]
fn installer_uses_version_from_environment() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let releases = directory.path().join("releases/download/v1.2.3");
    let payload = directory.path().join("payload");
    let install_dir = directory.path().join("bin");
    std::fs::create_dir_all(&releases)?;
    std::fs::create_dir(&payload)?;

    let binary = payload.join("s");
    std::fs::write(&binary, "#!/bin/sh\nprintf 's 1.2.3\\n'\n")?;
    std::fs::set_permissions(&binary, Permissions::from_mode(0o755))?;

    let asset = format!("s-v1.2.3-{}.zip", current_target()?);
    let archive = releases.join(&asset);
    let status = Command::new("zip")
        .args(["-j"])
        .arg(&archive)
        .arg(&binary)
        .status()
        .context("create test release archive")?;
    ensure!(status.success(), "zip failed with {status}");

    let commands = directory.path().join("commands");
    std::fs::create_dir(&commands)?;
    let curl = commands.join("curl");
    std::fs::write(
        &curl,
        "#!/bin/sh\nresolve=false\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --write-out ]; then\n    resolve=true\n    shift\n  elif [ \"$1\" = --output ]; then\n    output=$2\n    shift\n  fi\n  shift\ndone\nif [ \"$resolve\" = true ]; then\n  printf 'https://github.com/ma233/s-is-symbol/releases/tag/v1.2.3'\nelse\n  cp \"$SYMBOL_TEST_ARCHIVE\" \"$output\"\nfi\n",
    )?;
    std::fs::set_permissions(&curl, Permissions::from_mode(0o755))?;
    let path = format!(
        "{}:{}",
        commands.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let status = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .arg("--install-dir")
        .arg(&install_dir)
        .env("PATH", path)
        .env("SYMBOL_VERSION", "v1.2.3")
        .env("SYMBOL_TEST_ARCHIVE", &archive)
        .status()
        .context("run installer")?;
    ensure!(status.success(), "installer failed with {status}");

    let output = Command::new(install_dir.join("s"))
        .arg("--version")
        .output()?;
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout)?, "s 1.2.3\n");
    Ok(())
}

fn current_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-musl"),
        (arch, os) => anyhow::bail!("unsupported test platform: {arch}-{os}"),
    }
}
