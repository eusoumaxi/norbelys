//! Inbox process composition: configure the reader and supervise scheduling with health.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use jiff::SignedDuration;
use norbelys_mail::net::{AddressPolicy, Connector};
use tokio::sync::Semaphore;

use crate::config::InboxArgs;
use crate::delivery::limits::{Limits, Role, Shares, Split};
use crate::inbox::poll::{self, Reader};
use crate::process::{self, Shutdown};
use crate::senders;
use crate::senders::tokens::Tokens;
use crate::storage::Storage;

pub async fn run(args: InboxArgs) -> anyhow::Result<()> {
    let db = super::connect(
        &args.common,
        super::background_pool("norbelys-inbox", args.pool_size, Duration::from_secs(10)),
    )
    .await?;
    let keys = super::role_keys(&args.common, crate::config::KeyRole::Inbox)?;
    let resolver = crate::dns::Resolver::system().context("the DNS resolver cannot be built")?;
    let mail = senders::Settings::from_args(&args.mail)?;
    let policy = if args.mail.mail_allow_private_hosts {
        tracing::warn!(
            "mailbox hosts may be private addresses and plaintext: a development setting"
        );
        AddressPolicy::Any
    } else {
        AddressPolicy::PublicOnly
    };
    let reader = Arc::new(Reader {
        tokens: Tokens::new(keys.clone(), mail.http.clone(), mail.apps.clone()),
        keys,
        http: mail.http,
        connector: Connector::new(resolver.hickory(), policy)
            .context("the TLS configuration of IMAP sessions cannot be built")?,
        storage: Storage::from_args(&args.storage, &args.common.environment)?,
        interval: SignedDuration::from_secs(i64::from(args.poll_interval_seconds.max(1))),
        limits: Limits::new(
            Shares::new(Role::Receiving, Split::from(args.shares), args.replicas)
                .context("the inbox's share of the provider rate limits is refused")?,
        ),
    });
    let permits = usize::try_from(args.permits.max(1)).unwrap_or(1);
    let slots = Arc::new(Semaphore::new(permits));
    let owner = poll::owner();
    let shutdown = Shutdown::watch_signal();
    tracing::info!(owner = %owner, permits, "inbox started");

    let claiming = crate::inbox::runner::run(
        db.clone(),
        Arc::clone(&reader),
        Arc::clone(&slots),
        owner.clone(),
        shutdown.clone(),
    );
    process::supervise(
        "inbox.claim",
        claiming,
        "health",
        process::serve_health(args.common.health_addr, db, shutdown.clone()),
        shutdown,
    )
    .await
}
