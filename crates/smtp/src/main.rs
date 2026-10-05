//! `norbelys-smtp`: the control plane of Norbelys's own mail transfer agent, the `norbelys`
//! sending provider. It runs on the mail host beside docker-mailserver (Postfix for SMTP,
//! Dovecot for logins and IMAP, Rspamd for DKIM signing, the sender policy and rate limits),
//! as a separate deployable with canonical state in remote Turso; it never links the product's server.
//!
//! Three subcommands:
//!
//! - `serve` ([`serve`]): the unprivileged service. A control API the core calls over the
//!   private network, every request signed per Standard Webhooks (domains and their DNS
//!   records, logins, grants and catch-alls, evidence routes); admission control answering
//!   Postfix's policy requests; the collector that reads `mail.log`; the intake of bounces and
//!   feedback-loop complaints; the outbox that posts delivery events to the core in signed
//!   batches; a bounded local pending queue and confirmed remote archival; health routes;
//!   telemetry over OTLP.
//! - `provision-apply` ([`provision`]): the short-lived privileged helper. It applies the changes
//!   the service queued to docker-mailserver's files (logins, sender maps, catch-alls, DKIM
//!   keys) and exits; the service itself never holds that privilege.
//! - `dms-patch` ([`dms_patch`]): run inside the mail container at its start; patches
//!   docker-mailserver's account helper to publish its maps atomically and match accounts
//!   exactly.
//!
//! Configuration is read once, from flags or the environment, by [`config`], which lists every
//! variable.

mod admission;
mod archive;
mod bounce;
mod config;
mod control;
mod crypto;
mod db;
mod dms_patch;
mod events;
mod feedback;
mod health;
mod provision;
mod queue;
mod serve;
mod tail;
mod telemetry;
#[cfg(test)]
mod testing;

use std::time::Duration;

use clap::Parser as _;

use crate::config::{Cli, Command, ServeArgs};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Telemetry starts before any runtime and stops after it: its exporters block.
    let telemetry = telemetry::init(cli.command.name(), cli.command.telemetry())?;
    let result = match cli.command {
        Command::Serve(args) => serve_blocking(args),
        Command::ProvisionApply(args) => provision::apply(&args),
        Command::DmsPatch(args) => dms_patch::run(&args.file).map_err(anyhow::Error::from),
    };
    if let Err(error) = &result {
        tracing::error!(error = %format_args!("{error:#}"), "stopped with an error");
    }
    telemetry.shutdown();
    result
}

/// Runs `serve` on a multi-threaded runtime; blocking work still running at the end (a
/// remote SQL request) gets ten seconds.
fn serve_blocking(args: Box<ServeArgs>) -> anyhow::Result<()> {
    serve::validate(&args)?;
    let lock = serve::lock(&args.common.state_dir)?;
    let queue = queue::Queue::open(
        &args.common.queue(),
        &args.common.node,
        args.queue_max_bytes,
    )?;
    // This owner outlives the async runtime; the blocking HTTP client's final drop is synchronous.
    let database = db::Db::remote(&args.common.database, &args.common.node, None)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve::run(args, database.clone(), queue, lock));
    runtime.shutdown_timeout(Duration::from_secs(10));
    result
}
