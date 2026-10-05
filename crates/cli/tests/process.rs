//! Verify the installed command's exit status, streams and profile storage in child processes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("norbelys-process-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norbelys"));
    command
        .current_dir(directory)
        .env_remove("NORBELYS_API_KEY")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env_remove("APPDATA");
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_failure(output: &Output, code: i32, message: &str) {
    assert_eq!(output.status.code(), Some(code), "{}", text(&output.stderr));
    assert!(output.stdout.is_empty());
    assert!(
        text(&output.stderr).contains(message),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn help_and_version_succeed_without_credentials_or_a_home_directory() {
    let scratch = Scratch::new().unwrap();
    for (arguments, expected) in [
        (vec!["--help"], "Exit codes:"),
        (vec!["people", "list", "--help"], "Usage:"),
        (
            vec!["--version"],
            concat!("norbelys ", env!("CARGO_PKG_VERSION")),
        ),
    ] {
        let output = command(&scratch.0).args(arguments).output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(text(&output.stdout).contains(expected));
    }
}

#[test]
fn invalid_arguments_and_configuration_fail_before_any_request() {
    let scratch = Scratch::new().unwrap();
    let config = scratch.0.join("config.json");
    let output = command(&scratch.0).arg("unknown-command").output().unwrap();
    assert_failure(&output, 2, "unrecognized subcommand");

    let output = command(&scratch.0)
        .args(["people", "list"])
        .output()
        .unwrap();
    assert_failure(&output, 1, "no configuration directory");

    std::fs::write(&config, "{broken").unwrap();
    let output = command(&scratch.0)
        .arg("--config")
        .arg(&config)
        .args(["people", "list"])
        .output()
        .unwrap();
    assert_failure(&output, 1, "is not valid");

    std::fs::remove_file(&config).unwrap();
    let output = command(&scratch.0)
        .arg("--config")
        .arg(&config)
        .args(["people", "list"])
        .output()
        .unwrap();
    assert_failure(&output, 3, "norbelys login");
}

#[test]
fn default_configuration_uses_xdg_then_home_then_appdata() {
    let scratch = Scratch::new().unwrap();
    for (variable, relative) in [
        ("XDG_CONFIG_HOME", "norbelys/config.json"),
        ("HOME", ".config/norbelys/config.json"),
        ("APPDATA", "norbelys/config.json"),
    ] {
        let directory = scratch.0.join(variable);
        std::fs::create_dir_all(&directory).unwrap();
        let output = command(&scratch.0)
            .env(variable, &directory)
            .args(["login", "--api-key", "nb_test_process", "--profile", "ci"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join(relative)).unwrap()).unwrap();
        assert_eq!(saved["profiles"]["ci"]["api_key"], "nb_test_process");
    }
    let xdg = scratch.0.join("preferred");
    let home = scratch.0.join("unused-home");
    let output = command(&scratch.0)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("HOME", &home)
        .args(["login", "--api-key", "nb_test_process"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(xdg.join("norbelys/config.json").exists());
    assert!(!home.exists());
}
