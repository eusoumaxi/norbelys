//! Sender process composition: dependencies, PostgreSQL notifications, health and shutdown.
//! The delivery feature owns claims, message execution, result batches and recovery.

use crate::config::SenderArgs;
use crate::delivery::limits::{Limits, Role, Shares, Split};
use crate::delivery::runner;
use crate::delivery::submit::{self, Transports};
use crate::dns::Resolver;
use crate::process::{self, Shutdown};
use crate::{rendering, senders};
use anyhow::Context as _;
use secrecy::ExposeSecret as _;
use sqlx::postgres::PgListener;
use std::time::Duration;

pub async fn run(args: SenderArgs) -> anyhow::Result<()> {
    let db = super::connect(
        &args.common,
        super::background_pool("norbelys-sender", args.pool_size, Duration::from_secs(10)),
    )
    .await?;
    let keys = super::role_keys(&args.common, crate::config::KeyRole::Sender)?;
    let resolver = Resolver::system().context("the DNS resolver cannot be built")?;
    let mail = senders::Settings::from_args(&args.mail)?;
    let transports = Transports::new(submit::Config {
        keys: keys.clone(),
        http: mail.http.clone(),
        apps: mail.apps.clone(),
        resolver: resolver.clone(),
        allow_private_hosts: args.mail.mail_allow_private_hosts,
    })?;
    if args.mail.mail_allow_private_hosts {
        tracing::warn!(
            "mailbox and relay hosts may be private addresses and plaintext: a development setting"
        );
    }
    let retry_window = Duration::from_secs(args.retry_window_hours.saturating_mul(3_600));
    let rendering =
        rendering::Settings::new(keys.clone(), &args.rendering.tracking_url, retry_window)?
            .with_storage(crate::storage::Storage::from_args(
                &args.storage,
                &args.common.environment,
            )?);
    let listen_url = args
        .common
        .database_url
        .clone()
        .context("DATABASE_URL is required by this role")?;
    let mut listener = PgListener::connect(listen_url.expose_secret())
        .await
        .context("cannot listen for delivery wake-ups")?;
    listener
        .listen(crate::jobs::CHANNEL)
        .await
        .context("cannot listen for delivery wake-ups")?;
    let shutdown = Shutdown::watch_signal();
    let mut sender = runner::start(
        runner::Settings {
            db: db.clone(),
            keys,
            resolver,
            rendering,
            transports,
            retry_window,
            permits: args.permits,
            limits: Limits::new(
                Shares::new(Role::Sending, Split::from(args.shares), args.replicas)
                    .context("the sender's share of the provider rate limits is refused")?,
            ),
        },
        listener,
        shutdown.clone(),
    );
    let result = process::supervise(
        "sender.loops",
        sender.wait(),
        "health",
        process::serve_health(args.common.health_addr, db, shutdown.clone()),
        shutdown,
    )
    .await;
    sender.drain().await;
    result
}
