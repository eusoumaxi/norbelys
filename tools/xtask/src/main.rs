//! `cargo xtask <command>`: the repository's own checks and chores, written in Rust and run
//! through the alias `xtask = "run --package xtask --"` in `.cargo/config.toml` (the
//! cargo-xtask pattern, <https://github.com/matklad/cargo-xtask>). The package is a member of
//! the workspace so it builds with the same toolchain and lockfile, and no product crate depends
//! on it, so nothing here is ever shipped.
//!
//! | Command | Checks or does | Fails when |
//! |---|---|---|
//! | `purity` | the imports and paths of `crates/server/src/domain/`, and every tracked file for Python ([`purity`]) | the pure policy names a database, HTTP, runtime, mail transport, environment or clock item; a maintenance file outside the Python SDK uses Python |
//! | `db reset` | drops and recreates the database `DATABASE_URL` names, empty, on the local server ([`db`]) | the URL is not a local database |
//! | `db migrate` | provisions local roles/passwords, then applies SQLx migrations ([`db`]) | the URL is not local, provisioning or schema creation fails |
//! | `provision` | provisions cluster roles using explicit maintenance credentials ([`migrate`]) | credentials or cluster privileges are missing |
//! | `migrate [--check]` | applies SQLx migrations, or checks their history without writes ([`migrate`]) | maintenance credentials are missing or the schema is incompatible |
//! | `openapi check` | the committed public OpenAPI document against the handlers, and every example in it against its schema ([`openapi`]) | a difference, an invalid example |
//! | `gates sender-crash`, `gates worker-crash`, `gates all` | the crash gates: the roles as processes on a fresh database, a sender killed mid-submission and a worker mid-chunk, recovery judged by pure decisions ([`gates`]); `gates stall-smtp` serves their stalling SMTP server alone | a recovery that does not hold: a message not `uncertain` or resent, a row imported twice or lost, a ledger off its attempts, a lane slot held |
//! | `storage smoke` | put, a ranged get, list, a multipart upload, a presigned GET and delete against the S3 bucket the deployment's variables name, through the server's own store ([`storage`]) | an operation fails, the variables name no `s3://` bucket, or the server's smoke test is gone |
//!
//! Each check prints one line per finding and exits with status 1 when it found any, so the same
//! command is a step of the completion gate (`turbo run check`), of CI and of a developer's
//! loop. Errors that stop a command before it can decide (an unreadable file, a database that
//! does not answer) exit with status 1 as well, with the error.

mod db;
mod gates;
mod migrate;
mod openapi;
mod purity;
mod storage;

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

/// `cargo xtask <command>`.
#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "The repository's own checks and chores (run as `cargo xtask <command>`)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Apply versioned SQLx migrations manually from an external maintenance environment.
    Migrate {
        /// Verify migration history without changing the database.
        #[arg(long)]
        check: bool,
    },
    /// Create cluster roles explicitly using MIGRATION_DATABASE_URL (cluster administrator).
    Provision,
    /// Fails when `crates/server/src/domain/` uses a database, HTTP, runtime, mail transport,
    /// environment or clock item, fully qualified paths included, or when maintenance tooling outside `sdks/python/` uses Python.
    Purity,
    /// The local development database.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// The OpenAPI document.
    Openapi {
        #[command(subcommand)]
        command: OpenapiCommand,
    },
    /// Object storage.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// The crash gates: the roles as processes on a fresh database, one killed and recovered.
    Gates {
        #[command(subcommand)]
        command: GatesCommand,
    },
}

/// `cargo xtask db …`.
#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Drops and recreates the database `DATABASE_URL` names, empty, for an explicit schema setup; local servers only.
    Reset,
    /// Provisions local roles, applies SQLx migrations, then sets development login passwords.
    Migrate,
}

/// `cargo xtask openapi …`.
#[derive(Debug, Subcommand)]
enum OpenapiCommand {
    /// Fails when the committed public document differs from the handlers or an example in it
    /// does not validate against its schema.
    Check,
}

/// `cargo xtask gates …`.
#[derive(Debug, Subcommand)]
enum GatesCommand {
    /// Fails unless a sender killed in the middle of a submission leaves its message `uncertain`,
    /// settled once and never resent.
    SenderCrash,
    /// Fails unless a worker killed in the middle of a job's chunk loses nothing and repeats
    /// nothing: the job resumes and every row of its import is imported once.
    WorkerCrash,
    /// Every crash gate, one after the other.
    All,
    /// Serves the gates' stalling SMTP server on 127.0.0.1 until interrupted, to try a sender by
    /// hand: it accepts everything and never answers the end of a message.
    StallSmtp {
        #[arg(long, default_value_t = gates::SMTP_PORT)]
        port: u16,
    },
}

/// `cargo xtask storage …`.
#[derive(Debug, Subcommand)]
enum StorageCommand {
    /// Fails when the S3 bucket the deployment's variables name (`OBJECT_STORE_URL` and the
    /// `AWS_*` settings) does not do every operation the product uses; it writes and deletes
    /// objects under `smoke/` in that bucket.
    Smoke,
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let root = root();
    let findings = match cli.command {
        Command::Migrate { check } => {
            migrate::run(check, false)?;
            Vec::new()
        }
        Command::Provision => {
            migrate::run(false, true)?;
            Vec::new()
        }
        Command::Purity => purity::run(&root)?,
        Command::Db {
            command: DbCommand::Reset,
        } => {
            db::reset()?;
            Vec::new()
        }
        Command::Db {
            command: DbCommand::Migrate,
        } => {
            db::migrate()?;
            Vec::new()
        }
        Command::Openapi {
            command: OpenapiCommand::Check,
        } => openapi::run(&root)?,
        Command::Storage {
            command: StorageCommand::Smoke,
        } => storage::smoke(&root)?,
        Command::Gates { command } => match command {
            GatesCommand::SenderCrash => gates::sender_crash(&root)?,
            GatesCommand::WorkerCrash => gates::worker_crash(&root)?,
            GatesCommand::All => gates::all(&root)?,
            GatesCommand::StallSmtp { port } => {
                gates::stall_smtp(port)?;
                Vec::new()
            }
        },
    };
    report(&findings)
}

/// The repository root: this package lives at `tools/xtask`.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Prints every finding on its own line and answers the exit status: success when there is none.
fn report(findings: &[String]) -> anyhow::Result<ExitCode> {
    let mut out = io::stdout().lock();
    for finding in findings {
        writeln!(out, "{finding}")?;
    }
    if findings.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        writeln!(out, "{} finding(s)", findings.len())?;
        Ok(ExitCode::FAILURE)
    }
}

/// Runs `git` with `args` at the repository root and answers what it printed on its standard
/// output.
///
/// # Errors
///
/// Git cannot be run, or it exits with an error, returned with what it printed on its standard
/// error.
fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        anyhow::bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `path` relative to the repository root, with `/` separators, for messages.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
