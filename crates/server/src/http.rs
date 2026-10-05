//! HTTP for the api role: the state every handler can reach, the request context, the
//! extractors that turn requests into types, and the routers.

pub mod context;
pub mod extract;
pub mod openapi;
pub mod ratelimit;
pub mod router;
pub mod versioning;

use std::sync::Arc;

use axum::extract::FromRef;

use crate::crypto::Keys;
use crate::db::Database;
use crate::identity::authority::Authority;

/// Settings the handlers read.
#[derive(Debug)]
pub struct Settings {
    /// The public origin of the API, for absolute links.
    pub public_api_url: url::Url,
    /// The Sending area's settings: OAuth apps, webhook and tracking hosts, the managed MTA.
    pub senders: crate::senders::Settings,
    /// The public origin of the tracking host, where uploaded images are linked.
    pub public_tracking_url: url::Url,
}

/// What every handler can reach.
#[derive(Clone, Debug)]
pub struct AppState {
    pub db: Database,
    pub keys: Keys,
    pub authority: Authority,
    pub settings: Arc<Settings>,
    /// The object store: import files, import error reports, exports.
    pub storage: crate::storage::Storage,
    /// The process's one DNS resolver, for preflight.
    pub resolver: crate::dns::Resolver,
    /// Sign-in and the dashboard surface: settings, signing keys, the identity fetcher, the
    /// captcha.
    pub identity: crate::identity::Identity,
    /// The rate limiters of this process.
    pub limits: ratelimit::Limits,
    /// The provider-webhook ingress: its micro-batcher, its spool and the SNS certificates.
    pub ingress: crate::webhooks::ingress::Ingress,
}

impl FromRef<AppState> for Database {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

impl FromRef<AppState> for Keys {
    fn from_ref(state: &AppState) -> Self {
        state.keys.clone()
    }
}
