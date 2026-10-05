//! The api role: the product surface (the `/v1` API) or the ingress surface (provider webhooks,
//! unsubscribes, public images).
//!
//! Both modes serve the provider-webhook ingress, so each process opens the ingress's local spool
//! (`INGRESS_SPOOL_DIR`) and runs its drain beside the listener: callbacks the database could not
//! take wait on disk and are stored once it is back. On `SIGTERM` the listener stops taking
//! requests, the drain stores what it can within [`DRAIN_GRACE`], and the spool is closed; what is
//! left waits for the next start.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;

use crate::config::{ApiArgs, ApiMode, KeyRole};
use crate::db::PoolSettings;
use crate::http::{AppState, Settings, router};
use crate::process::Shutdown;
use crate::spool::{Limits, Spool};
use crate::webhooks::ingress::{self, Ingress};

/// How long the ingress drain may go on storing callbacks once the role is asked to stop.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

pub async fn run(args: ApiArgs) -> anyhow::Result<()> {
    let (application_name, key_role, pool_size) = match args.mode {
        ApiMode::Product => ("norbelys-api", KeyRole::Api, args.pool_size.unwrap_or(8)),
        // The ingress mode answers providers and recipients on the public host: a smaller pool,
        // and only the subkeys its routes use.
        ApiMode::Ingress => (
            "norbelys-ingress",
            KeyRole::Ingress,
            args.pool_size.unwrap_or(4),
        ),
    };
    let db = super::connect(
        &args.common,
        PoolSettings {
            application_name,
            max_connections: pool_size,
            statement_timeout: Duration::from_secs(5),
            acquire_timeout: Duration::from_secs(5),
            min_connections: pool_size,
            // Fail early with 503 rather than queue behind a saturated pool.
            request_deadline: Some(Duration::from_millis(50)),
        },
    )
    .await?;
    let dir = super::spool_dir(
        args.ingress_spool_dir.as_deref(),
        &args.common,
        "INGRESS_SPOOL_DIR",
        "norbelys-ingress-spool",
    )?;
    let spool = Spool::open(&dir, Limits::default())
        .with_context(|| format!("cannot open the ingress spool in {}", dir.display()))?;
    let shutdown = Shutdown::watch_signal();
    // `/metrics` on the health address: the api's probes answer on its own listener.
    crate::process::spawn_exporter(
        "metrics.listener",
        crate::process::serve_metrics(args.common.health_addr, shutdown.clone()),
        shutdown.clone(),
    );
    let mut draining = tokio::spawn(ingress::drain(db.clone(), spool.clone(), shutdown.clone()));
    let state = AppState {
        db: db.clone(),
        keys: super::role_keys(&args.common, key_role)?,
        authority: crate::identity::authority::Authority::with_ttl(Duration::from_secs(
            args.identity.authority_cache_seconds,
        )),
        settings: Arc::new(Settings {
            public_api_url: args.public_api_url,
            senders: crate::senders::Settings::from_args(&args.mail)?,
            public_tracking_url: args.rendering.tracking_url,
        }),
        storage: crate::storage::Storage::from_args(&args.storage, &args.common.environment)?,
        resolver: crate::dns::Resolver::system()?,
        identity: crate::identity::Identity::from_args(&args.identity, &args.mail)?,
        limits: crate::http::ratelimit::Limits::new(
            args.identity.trust_forwarded_for,
            args.replicas,
            args.identity.trusted_proxy_ips.clone(),
        ),
        ingress: Ingress::start(db, Some(spool.clone())),
    };
    let app = match args.mode {
        ApiMode::Product => {
            // Only the product surface authenticates, so only it can deny one address repeatedly.
            crate::identity::authority::observe_denials();
            router::product(state)
        }
        ApiMode::Ingress => router::ingress(state),
    };
    let listener = tokio::net::TcpListener::bind(args.http_addr).await?;
    tracing::info!(addr = %args.http_addr, mode = ?args.mode, spool = %dir.display(), "api listening");
    let mut stop = shutdown.clone();
    let serving = async {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.wait().await })
        .await?;
        Ok(())
    };
    let mut drain_stop = shutdown.clone();
    let drain_task = async {
        tokio::select! {
            joined = &mut draining => joined.map_err(|_| anyhow::anyhow!("spool drain panicked or was cancelled")),
            () = drain_stop.wait() => {
                match tokio::time::timeout(DRAIN_GRACE, &mut draining).await {
                    Ok(joined) => joined.map_err(|_| anyhow::anyhow!("spool drain panicked during shutdown")),
                    Err(_) => Err(anyhow::anyhow!("spool drain exceeded its shutdown grace")),
                }
            }
        }
    };
    let result = crate::process::supervise(
        "http.listener",
        serving,
        "spool.drain",
        drain_task,
        shutdown,
    )
    .await;
    if !draining.is_finished() {
        draining.abort();
        let _ = draining.await;
    }
    spool.close().await;
    result
}
