//! The Norbelys backend: one application library and one runtime command dispatcher.
//!
//! Norbelys is a multi-tenant cold-email platform: workspaces connect mailboxes and
//! relays, write campaigns, send on a paced schedule, and read replies and delivery
//! evidence. This crate holds the whole product server; the protocol work lives in
//! `norbelys-mail` and the AI client in `norbelys-ai`.
//!
//! A domain folder holds operations, not layers; `domain/` is pure and imports no I/O.
//! Every mechanism (pagination, idempotency, problems, sealing, tenancy) has exactly one
//! module; extend it rather than writing a second one.

mod ai;
mod analytics;
mod campaigns;
mod config;
mod crypto;
mod db;
mod delivery;
mod dns;
mod domain;
mod http;
mod idempotency;
mod identity;
mod inbox;
mod jobs;
mod mcp;
mod pagination;
mod people;
mod problem;
mod process;
mod rendering;
mod roles;
mod senders;
mod spool;
mod storage;
mod telemetry;
#[cfg(test)]
mod testing;
mod tracking;
mod webhooks;

pub use config::{Cli, Role};

/// Runs the role the command line names, on a multi-threaded runtime, with telemetry
/// initialised first and flushed last.
///
/// # Errors
///
/// Whatever stops the role: invalid configuration, an unreachable database, a failed
/// listener.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    // The container health probe runs every few seconds in every container: it needs neither
    // a runtime nor telemetry, and must not appear in the role's signals.
    if let Role::Healthcheck(args) = &cli.role {
        return roles::healthcheck::probe(args);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let telemetry = telemetry::init(cli.role.name(), cli.role.common())?;
        std::panic::set_hook(Box::new(|info| {
            tracing::error!(
                error_code = "panic",
                file = info.location().map(std::panic::Location::file),
                line = info.location().map(std::panic::Location::line),
                "panic payload withheld"
            );
        }));
        let result = roles::run(cli.role).await;
        if let Err(error) = &result {
            tracing::error!(error = %error, "role stopped with an error");
        }
        telemetry.shutdown();
        result
    })
}
