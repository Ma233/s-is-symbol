#![cfg(unix)]

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::ensure;

#[test]
fn installer_uses_version_from_environment() -> Result<()> {
    let release = TestRelease::new(false)?;

    let status = release.run_installer()?;
    ensure!(status.success(), "installer failed with {status}");

    let output = Command::new(release.install_dir.join("s"))
        .arg("--version")
        .output()?;
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout)?, "s 1.2.3\n");
    Ok(())
}

#[test]
fn installer_rejects_an_archive_with_the_wrong_checksum() -> Result<()> {
    let release = TestRelease::new(true)?;

    let status = release.run_installer()?;

    assert!(!status.success());
    assert!(!release.install_dir.join("s").exists());
    Ok(())
}

struct TestRelease {
    _directory: tempfile::TempDir,
    archive: std::path::PathBuf,
    checksum: std::path::PathBuf,
    commands: std::path::PathBuf,
    install_dir: std::path::PathBuf,
}

impl TestRelease {
    fn new(use_wrong_checksum: bool) -> Result<Self> {
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

        let checksum = releases.join(format!("{asset}.sha256"));
        let digest = if use_wrong_checksum {
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned()
        } else {
            sha256(&archive)?
        };
        std::fs::write(&checksum, format!("{digest}  {asset}\n"))?;

        let commands = directory.path().join("commands");
        std::fs::create_dir(&commands)?;
        let curl = commands.join("curl");
        std::fs::write(
            &curl,
            "#!/bin/sh\nresolve=false\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --write-out ]; then\n    resolve=true\n    shift\n  elif [ \"$1\" = --output ]; then\n    output=$2\n    shift\n  fi\n  shift\ndone\nif [ \"$resolve\" = true ]; then\n  printf 'https://github.com/ma233/s-is-symbol/releases/tag/v1.2.3'\nelse\n  case \"$output\" in\n    *.sha256) cp \"$SYMBOL_TEST_CHECKSUM\" \"$output\" ;;\n    *) cp \"$SYMBOL_TEST_ARCHIVE\" \"$output\" ;;\n  esac\nfi\n",
        )?;
        std::fs::set_permissions(&curl, Permissions::from_mode(0o755))?;

        Ok(Self {
            _directory: directory,
            archive,
            checksum,
            commands,
            install_dir,
        })
    }

    fn run_installer(&self) -> Result<std::process::ExitStatus> {
        let path = format!(
            "{}:{}",
            self.commands.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
            .arg("--install-dir")
            .arg(&self.install_dir)
            .env("PATH", path)
            .env("SYMBOL_VERSION", "v1.2.3")
            .env("SYMBOL_TEST_ARCHIVE", &self.archive)
            .env("SYMBOL_TEST_CHECKSUM", &self.checksum)
            .status()
            .context("run installer")
    }
}

fn sha256(path: &std::path::Path) -> Result<String> {
    let output = if cfg!(target_os = "macos") {
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
    } else {
        Command::new("sha256sum").arg(path).output()
    }
    .context("calculate test archive checksum")?;
    ensure!(output.status.success(), "checksum command failed");
    String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("checksum command returned no digest")
}

fn current_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-musl"),
        (arch, os) => anyhow::bail!("unsupported test platform: {arch}-{os}"),
    }
}
