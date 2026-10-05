//! Typed startup configuration for runtime roles and operator commands.
//! Clap reads flags and environment variables at the entry point; `.env.example` describes
//! these application settings. Telemetry libraries also read their standard exporter settings.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum};
use secrecy::SecretString;

use crate::domain::ai::{ModelEntry, ModelRef};
use crate::jobs::Queue;

/// `norbelys-server <role>`.
#[derive(Debug, Parser)]
#[command(
    name = "norbelys-server",
    version,
    about = "The Norbelys server: independent runtime roles and operator commands"
)]
pub struct Cli {
    #[command(subcommand)]
    pub role: Role,
}

/// Runtime roles, one-shot operator commands and health probes.
/// Runtime processes initialize their own dependencies and share durable work through
/// PostgreSQL and object storage rather than process memory.
#[derive(Debug, Subcommand)]
pub enum Role {
    /// The HTTP API: the product surface, or the public host's ingress surface (`--mode ingress`):
    /// provider webhooks, unsubscribes and public images.
    Api(ApiArgs),
    /// Claims due connections and submits their mail.
    Sender(SenderArgs),
    /// Polls receive bindings and records inbound mail.
    Inbox(InboxArgs),
    /// Runs the job runner's lanes, the outbox relay and the maintenance lane.
    Worker(WorkerArgs),
    /// Answers open and click tracking and drains its spool.
    Tracking(TrackingArgs),
    /// Reads archived partitions and writes report snapshots.
    Analytics(AnalyticsArgs),
    /// Operator commands.
    Admin(AdminArgs),
    /// Not a role: the container health probe. Sends `GET <url>` and exits 0 on a `2xx`
    /// answer, 1 otherwise, without starting a runtime or telemetry.
    Healthcheck(HealthcheckArgs),
}

impl Role {
    /// The role's name, used as the `norbelys.role` telemetry attribute.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Api(_) => "api",
            Self::Sender(_) => "sender",
            Self::Inbox(_) => "inbox",
            Self::Worker(_) => "worker",
            Self::Tracking(_) => "tracking",
            Self::Analytics(_) => "analytics",
            Self::Admin(_) => "admin",
            Self::Healthcheck(_) => "healthcheck",
        }
    }

    /// The settings every role shares.
    #[must_use]
    pub fn common(&self) -> &Common {
        match self {
            Self::Api(args) => &args.common,
            Self::Sender(args) => &args.common,
            Self::Inbox(args) => &args.common,
            Self::Analytics(args) => &args.common,
            Self::Worker(args) => &args.common,
            Self::Tracking(args) => &args.common,
            Self::Admin(args) => &args.common,
            Self::Healthcheck(args) => &args.common,
        }
    }
}

/// Settings shared by every role.
#[derive(Debug, Args)]
pub struct Common {
    /// The role's own database login: each role connects as a different PostgreSQL role
    /// with only the grants it needs.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: Option<SecretString>,
    /// Base64 of the 32-byte deployment key: it seals secrets and keys every HMAC. A role keeps
    /// only the subkeys of it that it uses.
    #[arg(long, env = "NORBELYS_DEPLOYMENT_KEY", hide_env_values = true)]
    pub deployment_key: Option<SecretString>,
    /// During a rotation of the deployment key, the previous one: what it sealed or signed still
    /// opens and verifies while `admin secrets rotate` re-seals the stored rows; nothing new is
    /// made with it.
    #[arg(long, env = "NORBELYS_PREVIOUS_DEPLOYMENT_KEY", hide_env_values = true)]
    pub previous_deployment_key: Option<SecretString>,
    /// The role's own key (`nbrk_…`, printed by `admin deployment-key --for <role>`): only the
    /// subkeys this role uses, read when `NORBELYS_DEPLOYMENT_KEY` is not set.
    #[arg(long, env = "NORBELYS_ROLE_KEY", hide_env_values = true)]
    pub role_key: Option<SecretString>,
    /// Where `/health/live`, `/health/ready` and `/metrics` answer (the api and tracking answer
    /// their probes on their own listener, and only `/metrics` here).
    #[arg(long, env = "HEALTH_ADDR", default_value = "127.0.0.1:8090")]
    pub health_addr: SocketAddr,
    /// The OTLP endpoint; without it, telemetry stays local (JSON lines on stdout).
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,
    /// Percentage of complete traces exported; metrics and error logs are never sampled.
    #[arg(long, env = "NORBELYS_TRACE_SAMPLE_PERCENT", default_value = "100", value_parser = clap::value_parser!(u8).range(0..=100))]
    pub trace_sample_percent: u8,
    /// The deployment's name in telemetry (`deployment.environment.name`).
    #[arg(long, env = "NORBELYS_ENVIRONMENT", default_value = "development")]
    pub environment: String,
}

/// Which surface an api process serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApiMode {
    /// The product: `/v1`, the dashboard surface and the protocols.
    Product,
    /// Provider webhooks, unsubscribes (the page and RFC 8058's one-click `POST`) and public
    /// images only, on the public host.
    Ingress,
}

/// What a role key is made for (`admin deployment-key --for <role>`): each holds the subkeys of
/// the deployment key that this process's work uses, and no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, strum::EnumIter)]
pub enum KeyRole {
    /// The api in product mode, and the operator's `admin`: every subkey.
    Api,
    /// The api in `--mode ingress`: opening provider webhooks' keys, verifying unsubscribe links.
    Ingress,
    /// Opening and refreshing mailbox credentials, signing the links of the mail it renders.
    Sender,
    /// Opening and refreshing mailbox credentials, recognising our Message-IDs in replies.
    Inbox,
    /// What the job kinds use: credentials and webhook secrets, links, Message-IDs.
    Worker,
    /// Verifying open and click tokens, hashing client addresses; never the sealing key.
    Tracking,
}

/// The api role.
#[derive(Debug, Args)]
pub struct ApiArgs {
    #[command(flatten)]
    pub common: Common,
    /// The surface this process serves.
    #[arg(long, env = "API_MODE", value_enum, default_value_t = ApiMode::Product)]
    pub mode: ApiMode,
    /// The HTTP listener.
    #[arg(long, env = "HTTP_ADDR", default_value = "127.0.0.1:3001")]
    pub http_addr: SocketAddr,
    /// The public origin of this API, for absolute links and `Location` headers.
    #[arg(long, env = "PUBLIC_API_URL", default_value = "http://127.0.0.1:3001")]
    pub public_api_url: url::Url,
    /// Database connections for this process: when unset, 8 for the product, whose pool is
    /// reserved for its requests, and 4 for the ingress mode.
    #[arg(long, env = "DATABASE_POOL_SIZE")]
    pub pool_size: Option<u32>,
    #[command(flatten)]
    pub mail: MailArgs,
    #[command(flatten)]
    pub storage: StorageArgs,
    #[command(flatten)]
    pub identity: IdentityArgs,
    /// The tracking host, where uploaded images are linked (`<origin>/images/…`).
    #[command(flatten)]
    pub rendering: RenderingArgs,
    /// The directory of the provider-webhook ingress's spool, where callbacks wait on disk while
    /// the database cannot take them; on a disk that survives restarts. Without it a
    /// `development` deployment keeps the spool under the system's temporary directory; any
    /// other deployment refuses to start.
    #[arg(long, env = "INGRESS_SPOOL_DIR")]
    pub ingress_spool_dir: Option<std::path::PathBuf>,
    /// The api replicas the deployment runs. Each holds this fraction of the per-workspace and
    /// per-person request budgets (the `RateLimit` policies `default`, `send` and `access`), so
    /// together they admit about the budget.
    #[arg(long, env = "API_REPLICAS", default_value_t = 1)]
    pub replicas: u32,
}

/// The Sending area's settings, shared by the api (connecting mailboxes, showing DNS records and
/// webhook URLs) and the worker (checking credentials, provisioning the managed MTA, verifying
/// domains).
#[derive(Debug, Args)]
pub struct MailArgs {
    /// Norbelys's Google OAuth client id; without it and its secret, Google connections are
    /// refused.
    #[arg(long, env = "GOOGLE_OAUTH_CLIENT_ID")]
    pub google_oauth_client_id: Option<String>,
    /// The Google OAuth client's secret.
    #[arg(long, env = "GOOGLE_OAUTH_CLIENT_SECRET", hide_env_values = true)]
    pub google_oauth_client_secret: Option<SecretString>,
    /// Norbelys's Microsoft app (client) id; without it and its secret, Microsoft connections
    /// are refused.
    #[arg(long, env = "MICROSOFT_OAUTH_CLIENT_ID")]
    pub microsoft_oauth_client_id: Option<String>,
    /// The Microsoft app's client secret.
    #[arg(long, env = "MICROSOFT_OAUTH_CLIENT_SECRET", hide_env_values = true)]
    pub microsoft_oauth_client_secret: Option<SecretString>,
    /// The Microsoft identity platform's tenant segment: `common` admits work, school and
    /// personal accounts; a tenant id admits one organisation.
    #[arg(long, env = "MICROSOFT_OAUTH_TENANT", default_value = "common")]
    pub microsoft_oauth_tenant: String,
    /// The one redirect URI registered with both providers: `GET /v1/auth/callback` as the
    /// browser reaches it (through the dashboard's proxy), an `https` URL.
    #[arg(long, env = "OAUTH_REDIRECT_URL")]
    pub oauth_redirect_url: Option<url::Url>,
    /// The public origin providers post their callbacks to: `<origin>/webhooks/{id}` on the
    /// ingress host.
    #[arg(
        long,
        env = "PUBLIC_WEBHOOKS_URL",
        default_value = "http://127.0.0.1:3001"
    )]
    pub public_webhooks_url: url::Url,
    /// The host a sending domain's tracking hostname points its CNAME at.
    #[arg(
        long,
        env = "TRACKING_CNAME_TARGET",
        default_value = "tracking.norbelys.localhost"
    )]
    pub tracking_cname_target: String,
    /// The managed MTA's submission host, written into `norbelys` connections.
    #[arg(
        long,
        env = "MTA_SUBMISSION_HOST",
        default_value = "smtp.norbelys.localhost"
    )]
    pub mta_submission_host: String,
    /// The managed MTA's control API on the private network; without it (and its secret),
    /// `norbelys` connections wait in `verifying`.
    #[arg(long, env = "MTA_CONTROL_URL")]
    pub mta_control_url: Option<url::Url>,
    /// Authorize HTTP to a literal private MTA IP inside an encrypted tunnel. All public
    /// destinations still require HTTPS; this option does not relax tenant SSRF guards.
    #[arg(long, env = "MTA_CONTROL_PRIVATE_TRANSPORT", default_value_t = false)]
    pub mta_control_private_transport: bool,
    /// The control API's per-installation Standard Webhooks secret (`whsec_…`).
    #[arg(long, env = "MTA_CONTROL_SECRET", hide_env_values = true)]
    pub mta_control_secret: Option<SecretString>,
    /// Let mailbox and relay hosts resolve to private and loopback addresses, and allow
    /// plaintext sessions to them. For a development deployment with local fakes only: in
    /// production it would let a tenant's host reach the internal network.
    #[arg(long, env = "MAIL_ALLOW_PRIVATE_HOSTS")]
    pub mail_allow_private_hosts: bool,
}

/// Object storage, shared by the api (import uploads, download links) and the worker (imports
/// and exports). It speaks the S3 API, so the provider is configuration: a bucket URL, an
/// endpoint, a region, credentials and the addressing style (Cloudflare R2, AWS S3, Hetzner and
/// self-hosted servers all fit). A local directory serves development and tests.
#[derive(Debug, Args)]
pub struct StorageArgs {
    /// `s3://bucket`, optionally with a key prefix (`s3://bucket/norbelys`), or
    /// `file:///absolute/directory`. Without it a `development` deployment keeps its objects in
    /// a directory under the system's temporary directory; any other deployment refuses to
    /// start.
    #[arg(long, env = "OBJECT_STORE_URL")]
    pub object_store_url: Option<url::Url>,
    /// The access key id of an `s3://` store.
    #[arg(long, env = "AWS_ACCESS_KEY_ID")]
    pub aws_access_key_id: Option<String>,
    /// The secret access key of an `s3://` store.
    #[arg(long, env = "AWS_SECRET_ACCESS_KEY", hide_env_values = true)]
    pub aws_secret_access_key: Option<SecretString>,
    /// The bucket's region. Cloudflare R2 takes `auto` and accepts `us-east-1` as its alias.
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    pub aws_region: String,
    /// The S3 endpoint of a provider other than AWS, such as
    /// `https://<account>.r2.cloudflarestorage.com`; AWS's own endpoint when absent.
    #[arg(long, env = "AWS_ENDPOINT_URL")]
    pub aws_endpoint_url: Option<String>,
    /// Address the bucket as a host name (`https://bucket.endpoint/key`) instead of a path
    /// (`https://endpoint/bucket/key`).
    #[arg(long, env = "AWS_VIRTUAL_HOSTED_STYLE_REQUEST")]
    pub aws_virtual_hosted_style_request: bool,
    /// Allow plain `http` to the endpoint: a local S3 server in development only.
    #[arg(long, env = "AWS_ALLOW_HTTP")]
    pub aws_allow_http: bool,
}

/// Sign-in, sessions and the dashboard surface of the api.
#[derive(Debug, Args)]
pub struct IdentityArgs {
    /// Reuse verified authority for this many seconds (0 to 60). Zero re-reads the primary
    /// database on every request; a positive value explicitly accepts that revocation window.
    #[arg(long, env = "AUTHORITY_CACHE_SECONDS", default_value_t = 0, value_parser = clap::value_parser!(u64).range(0..=60))]
    pub authority_cache_seconds: u64,
    /// The dashboard's origins, comma-separated: the only `Origin`s whose sign-in and
    /// cookie-authorised requests are accepted, and (the first) where links in mail point.
    #[arg(
        long,
        env = "DASHBOARD_ORIGINS",
        value_delimiter = ',',
        default_value = "http://localhost:5173"
    )]
    pub dashboard_origins: Vec<url::Url>,
    /// The WebAuthn relying party id of passkeys: the dashboard's registrable domain
    /// (`norbelys.com`). Passkeys are bound to it for good, so it never changes once set.
    #[arg(long, env = "WEBAUTHN_RP_ID", default_value = "localhost")]
    pub webauthn_rp_id: String,
    /// The captcha of the email-code challenge (`turnstile`, `hcaptcha` or `recaptcha`); without
    /// it there is no captcha.
    #[arg(long, env = "CAPTCHA_PROVIDER", value_enum)]
    pub captcha_provider: Option<crate::identity::captcha::Provider>,
    /// The captcha's public site key, given to the dashboard's widget.
    #[arg(long, env = "CAPTCHA_SITE_KEY")]
    pub captcha_site_key: Option<String>,
    /// The captcha's secret (for reCAPTCHA, an API key allowed to create assessments).
    #[arg(long, env = "CAPTCHA_SECRET", hide_env_values = true)]
    pub captcha_secret: Option<SecretString>,
    /// The Google Cloud project of a reCAPTCHA captcha.
    #[arg(long, env = "RECAPTCHA_PROJECT")]
    pub recaptcha_project: Option<String>,
    /// Honor a forwarding chain only from an address in TRUSTED_PROXY_IPS, selecting the
    /// first untrusted address from the right. Unlisted TCP peers retain their own address.
    #[arg(long, env = "TRUST_FORWARDED_FOR")]
    pub trust_forwarded_for: bool,
    /// Exact TCP proxy addresses allowed to supply forwarding headers. Unlisted peers and
    /// invalid chains retain their TCP address, even when forwarding is enabled.
    #[arg(long, env = "TRUSTED_PROXY_IPS", value_delimiter = ',')]
    pub trusted_proxy_ips: Vec<std::net::IpAddr>,
    /// Let identity providers' documents be fetched from private and loopback addresses over
    /// plain `http`. For a development deployment with a local provider only.
    #[arg(long, env = "IDENTITY_ALLOW_PRIVATE_ISSUERS")]
    pub identity_allow_private_issuers: bool,
    /// Turn the OAuth authorization server off: its routes (`/oauth/*` and its two metadata
    /// documents) answer `404`, and MCP clients and the command-line client authenticate with
    /// API keys. The state a deployment runs in while the authorization server's gate has not
    /// passed.
    #[arg(long, env = "OAUTH_SERVER_DISABLED")]
    pub oauth_server_disabled: bool,
}

/// The analytics role: the database it reads the archive's manifest from (`norbelys_system`),
/// and the object store holding the archive and its report snapshots.
#[derive(Debug, Args)]
pub struct AnalyticsArgs {
    #[command(flatten)]
    pub common: Common,
    /// Database connections for this process.
    #[arg(long, env = "DATABASE_POOL_SIZE", default_value_t = 2)]
    pub pool_size: u32,
    /// Where the archive is.
    #[command(flatten)]
    pub storage: StorageArgs,
}

/// The inbox role: how it reaches mailboxes, where it keeps what it reads, and how much it reads
/// at once.
#[derive(Debug, Args)]
pub struct InboxArgs {
    #[command(flatten)]
    pub common: Common,
    /// Database connections for this process: a poll holds one only to read its binding and to
    /// store its page, never across a provider's round trip, so four serve its 32 permits.
    #[arg(long, env = "DATABASE_POOL_SIZE", default_value_t = 4)]
    pub pool_size: u32,
    /// The OAuth apps (to refresh mailbox grants) and the hosts IMAP may reach.
    #[command(flatten)]
    pub mail: MailArgs,
    /// Where raw inbound messages are kept.
    #[command(flatten)]
    pub storage: StorageArgs,
    /// Mailboxes this process polls at once. A claim never takes a binding while none is free.
    #[arg(long, env = "INBOX_PERMITS", default_value_t = 32)]
    pub permits: u32,
    /// How often a binding is polled when nothing asks for sooner, in seconds: the reply
    /// latency the inbox is measured against.
    #[arg(long, env = "INBOX_POLL_INTERVAL_SECONDS", default_value_t = 300)]
    pub poll_interval_seconds: u32,
    /// The inbox replicas the deployment runs: each holds this fraction of receiving's share of
    /// the provider rate limits. Raised before a replica is added, lowered after one is removed;
    /// the role refuses to start with less than one.
    #[arg(long, env = "INBOX_REPLICAS", default_value_t = 1)]
    pub replicas: u32,
    /// How the provider rate limits the roles share are split between them.
    #[command(flatten)]
    pub shares: LimitShareArgs,
}

/// The sender role: what it renders with, how it reaches providers, and how much it does at once.
#[derive(Debug, Args)]
pub struct SenderArgs {
    /// Shared object storage for message attachments.
    #[command(flatten)]
    pub storage: StorageArgs,
    #[command(flatten)]
    pub common: Common,
    /// Database connections for this process: a submission holds one for its Start and its
    /// Finish only, never across the provider's answer, so four serve its permits.
    #[arg(long, env = "DATABASE_POOL_SIZE", default_value_t = 4)]
    pub pool_size: u32,
    /// How mail is rendered.
    #[command(flatten)]
    pub rendering: RenderingArgs,
    /// The OAuth apps (to refresh mailbox grants) and the hosts the transports may reach.
    #[command(flatten)]
    pub mail: MailArgs,
    /// Messages this process prepares and submits at once: its submission slots. A claim never
    /// takes work while none is free.
    #[arg(long, env = "SENDER_PERMITS", default_value_t = 32)]
    pub permits: u32,
    /// The sender replicas the deployment runs. Each replica holds this fraction of every
    /// provider rate limit, so together they stay within it; the role refuses to start with less
    /// than one.
    #[arg(long, env = "SENDER_REPLICAS", default_value_t = 1)]
    pub replicas: u32,
    /// How the provider rate limits the roles share are split between them.
    #[command(flatten)]
    pub shares: LimitShareArgs,
    /// How long after its first submission a message keeps being tried, in hours.
    #[arg(long, env = "DELIVERY_RETRY_WINDOW_HOURS", default_value_t = 24)]
    pub retry_window_hours: u64,
}

/// How the provider rate limits that several roles consume (Gmail API units, Microsoft Graph
/// requests) are split between sending, receiving and maintenance, each a fraction from 0 to 1.
/// Every role that calls the providers reads the whole split, so each refuses to start when the
/// fractions sum above the whole limit. A limit only sending consumes (Exchange's messages a
/// minute, a relay's rate) is sending's whole and not split.
#[derive(Debug, Clone, Copy, Args)]
pub struct LimitShareArgs {
    /// Sending's share: submissions.
    #[arg(
        long = "sender-limit-share",
        env = "SENDER_LIMIT_SHARE",
        default_value_t = 0.8
    )]
    pub sending: f64,
    /// Receiving's share: the inbox's reads of mailboxes.
    #[arg(
        long = "inbox-limit-share",
        env = "INBOX_LIMIT_SHARE",
        default_value_t = 0.15
    )]
    pub receiving: f64,
    /// Maintenance's share: connection checks and the Sent-folder searches that settle
    /// uncertain messages.
    #[arg(
        long = "maintenance-limit-share",
        env = "MAINTENANCE_LIMIT_SHARE",
        default_value_t = 0.05
    )]
    pub maintenance: f64,
}

/// What rendering mail needs besides the database: the platform's tracking host. Read by the
/// sender, which renders every message it submits, by `admin render-message`, and by the api,
/// which links the images a workspace uploads on the same host.
#[derive(Debug, Args)]
pub struct RenderingArgs {
    /// The public origin of the tracking host, an `https` URL: the open pixels, click links and
    /// unsubscribe links of campaign mail (`<origin>/t/…`, `<origin>/u/…`) when the campaign has
    /// no tracking domain of its own. `List-Unsubscribe` accepts only `https`.
    #[arg(
        long,
        env = "PUBLIC_TRACKING_URL",
        default_value = "https://tracking.norbelys.localhost"
    )]
    pub tracking_url: url::Url,
}

/// The worker role.
#[derive(Debug, Args)]
pub struct WorkerArgs {
    #[command(flatten)]
    pub common: Common,
    /// Database connections for the tenant lanes.
    #[arg(long, env = "DATABASE_POOL_SIZE", default_value_t = 6)]
    pub pool_size: u32,
    /// The maintenance lane's own login, `norbelys_system`, used by the system job kinds.
    #[arg(long, env = "SYSTEM_DATABASE_URL", hide_env_values = true)]
    pub system_database_url: Option<SecretString>,
    /// The PostgreSQL metrics scraper's own login, `norbelys_metrics` (`pg_monitor`, no table
    /// grants), on one dedicated connection. Without it the `pg_*` metrics are not read.
    #[arg(long, env = "METRICS_DATABASE_URL", hide_env_values = true)]
    pub metrics_database_url: Option<SecretString>,
    /// How many jobs of a queue this process runs at once, where it differs from the queue's
    /// default: comma-separated `queue=permits` pairs, such as `webhooks=32,maintenance=2`.
    /// Size it to what the host's memory and the pool allow; a claim never takes more jobs than
    /// it has free permits.
    #[arg(long, env = "JOB_PERMITS", value_delimiter = ',', value_parser = parse_permits)]
    pub job_permits: Vec<(Queue, u32)>,
    /// Deliver customer webhooks to private, loopback and reserved addresses, and over plain
    /// `http`. For a development deployment with a local receiver only: in production it
    /// would let a customer's URL reach the internal network.
    #[arg(long, env = "WEBHOOK_ALLOW_PRIVATE_TARGETS")]
    pub webhook_allow_private_targets: bool,
    /// Let the daily check of SSO connections fetch identity providers' documents from private
    /// and loopback addresses over plain `http`. For a development deployment only.
    #[arg(long, env = "IDENTITY_ALLOW_PRIVATE_ISSUERS")]
    pub identity_allow_private_issuers: bool,
    /// The worker replicas the deployment runs: each holds this fraction of maintenance's share
    /// of the provider rate limits (connection checks). Raised before a replica is added,
    /// lowered after one is removed; the role refuses to start with less than one.
    #[arg(long, env = "WORKER_REPLICAS", default_value_t = 1)]
    pub replicas: u32,
    /// How the provider rate limits the roles share are split between them.
    #[command(flatten)]
    pub shares: LimitShareArgs,
    #[command(flatten)]
    pub mail: MailArgs,
    #[command(flatten)]
    pub storage: StorageArgs,
    #[command(flatten)]
    pub ai: AiArgs,
}

/// Parses one `queue=permits` pair of `JOB_PERMITS`.
fn parse_permits(pair: &str) -> Result<(Queue, u32), String> {
    let (queue, permits) = pair
        .split_once('=')
        .ok_or_else(|| format!("`{pair}` is not `queue=permits`"))?;
    let queue = queue
        .trim()
        .parse::<Queue>()
        .map_err(|_| format!("`{queue}` is not a job queue"))?;
    let permits = permits
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|permits| *permits > 0)
        .ok_or_else(|| format!("`{permits}` is not a positive number of permits"))?;
    Ok((queue, permits))
}

/// The catalogue of models a deployment may use, as read on 2026-10-01 from the providers'
/// pricing pages (USD per million input and output tokens, standard tier): Claude Haiku 4.5,
/// Sonnet 5.5 and Opus 5.5, and OpenAI's `gpt-6-luna`, `gpt-6.1-sol` and `gpt-6-astra`. The
/// capability matrix of the AI client marks each strict on its own provider's wire (Anthropic's
/// `output_config.format`, OpenAI's `response_format` with `strict: true`).
const DEFAULT_AI_MODELS: &str = "anthropic/claude-haiku-4-5=1:5,anthropic/claude-sonnet-5-5=2:10,anthropic/claude-opus-5-5=4:20,openai/gpt-6-luna=0.10:0.50,openai/gpt-6.1-sol=2:10,openai/gpt-6-astra=10:50";

/// The AI providers, models and prices of the worker, which makes every AI call as a job of the
/// `ai` queue.
///
/// Each provider has one deployment key; without a provider's key, the use cases configured on
/// its models are unavailable and their jobs keep the decision they make without AI. Model ids
/// and prices change often, so they are configuration: the catalogue prices every model the
/// deployment may use, and each use case names its model. Whether a provider enforces a JSON
/// schema for a model is not configuration but the tested capability matrix of the AI client
/// (`norbelys_ai::matrix`). A use case configured with a model the catalogue does not price, or
/// one the matrix does not mark strict, stops the worker at start rather than failing every one
/// of its jobs.
#[derive(Debug, Args)]
pub struct AiArgs {
    /// Anthropic's API key, for the `anthropic/…` models.
    #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
    pub anthropic_api_key: Option<SecretString>,
    /// The Anthropic API's base URL, as Anthropic's SDKs take it (without `/v1`).
    #[arg(
        long,
        env = "ANTHROPIC_BASE_URL",
        default_value = "https://api.anthropic.com"
    )]
    pub anthropic_base_url: url::Url,
    /// The key of OpenAI's API, or of a server that speaks OpenAI's chat completions, for the
    /// `openai/…` models (a self-hosted server that wants no key takes any value).
    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
    pub openai_api_key: Option<SecretString>,
    /// The chat completions base URL, with its version: OpenAI's, or a self-hosted server's
    /// (`http://127.0.0.1:11434/v1` for Ollama). Plain `http` is accepted only to a loopback or
    /// private address, since the key travels in a header.
    #[arg(
        long,
        env = "OPENAI_BASE_URL",
        default_value = "https://api.openai.com/v1"
    )]
    pub openai_base_url: url::Url,
    /// The catalogue, comma-separated: `provider/model=input:output`, prices in US dollars per
    /// million tokens. A call's prices are copied into its record when it is reserved.
    #[arg(long, env = "AI_MODELS", value_delimiter = ',', default_value = DEFAULT_AI_MODELS)]
    pub ai_models: Vec<ModelEntry>,
    /// The day the catalogue's prices were read from the providers' pages; a warning is logged
    /// at start when it is more than 90 days old.
    #[arg(long, env = "AI_PRICES_READ_ON", default_value = "2026-10-01")]
    pub ai_prices_read_on: jiff::civil::Date,
    /// The model that classifies inbound replies: the cheapest capable one.
    #[arg(
        long,
        env = "AI_CLASSIFICATION_MODEL",
        default_value = "anthropic/claude-haiku-4-5"
    )]
    pub ai_classification_model: ModelRef,
    /// The model that writes personalisation snippets.
    #[arg(
        long,
        env = "AI_SNIPPETS_MODEL",
        default_value = "anthropic/claude-sonnet-5-5"
    )]
    pub ai_snippets_model: ModelRef,
    /// The model of list hygiene hints (evaluated, not yet called by any job).
    #[arg(
        long,
        env = "AI_HINTS_MODEL",
        default_value = "anthropic/claude-haiku-4-5"
    )]
    pub ai_hints_model: ModelRef,
    /// The deadline of one AI call, its retries included, in seconds: 1 to 90.
    #[arg(
        long,
        env = "AI_TIMEOUT_SECONDS",
        default_value_t = 60,
        value_parser = clap::value_parser!(u64).range(1..=90)
    )]
    pub ai_timeout_seconds: u64,
    /// How far, in points of review rate (0 to 100), a canary prompt's review rate may exceed the
    /// current prompt's before the canary is rolled back. A new prompt version of classification
    /// first serves 5 % of the calls for a day; a rise beyond this margin ends it.
    #[arg(
        long,
        env = "AI_CANARY_MARGIN_POINTS",
        default_value_t = 5,
        value_parser = clap::value_parser!(u32).range(0..=100)
    )]
    pub ai_canary_margin_points: u32,
}

/// The tracking role.
#[derive(Debug, Args)]
pub struct TrackingArgs {
    #[command(flatten)]
    pub common: Common,
    /// The HTTP listener for opens and clicks, and the role's `/health/live` and `/health/ready`.
    #[arg(long, env = "HTTP_ADDR", default_value = "127.0.0.1:8081")]
    pub http_addr: SocketAddr,
    /// Database connections for the drain.
    #[arg(long, env = "DATABASE_POOL_SIZE", default_value_t = 2)]
    pub pool_size: u32,
    /// The directory of the role's spool, where events wait on disk until the drain has stored
    /// them; on a disk that survives restarts. Without it a `development` deployment keeps its
    /// spool under the system's temporary directory; any other deployment refuses to start.
    #[arg(long, env = "TRACKING_SPOOL_DIR")]
    pub spool_dir: Option<std::path::PathBuf>,
    /// Take the client's address from the leftmost `X-Forwarded-For`, which the proxy in front of
    /// the role sets; without such a proxy a client could choose the address recorded with its
    /// events.
    #[arg(long, env = "TRUST_FORWARDED_FOR")]
    pub trust_forwarded_for: bool,
    /// Exact TCP proxy addresses allowed to supply forwarding headers. Unlisted peers and
    /// invalid chains retain their TCP address, even when forwarding is enabled.
    #[arg(long, env = "TRUSTED_PROXY_IPS", value_delimiter = ',')]
    pub trusted_proxy_ips: Vec<std::net::IpAddr>,
}

/// The health probe: `norbelys-server healthcheck http://127.0.0.1:8080/health/ready`.
#[derive(Debug, Args)]
pub struct HealthcheckArgs {
    /// Accepted like every role's, and unused: the probe reads nothing else from the
    /// environment.
    #[command(flatten)]
    pub common: Common,
    /// The health route to probe, over plain HTTP: `/health/ready` on the role's own port, on
    /// the loopback interface of the container it runs in.
    pub url: url::Url,
    /// How long to wait to connect, and then for the answer, in milliseconds.
    #[arg(long, default_value_t = 3000)]
    pub timeout_ms: u64,
}

/// The admin role.
#[derive(Debug, Args)]
pub struct AdminArgs {
    #[command(flatten)]
    pub common: Common,
    #[command(subcommand)]
    pub command: AdminCommand,
}

/// Operator commands.
#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Verifies externally applied SQLx history without changing the database.
    SchemaCheck,
    /// Prints a fresh deployment key (base64 of 32 random bytes); with `--for`, the role key of
    /// one role instead: the subkeys that role uses of the configured `NORBELYS_DEPLOYMENT_KEY`,
    /// and of `NORBELYS_PREVIOUS_DEPLOYMENT_KEY` during a rotation, for the role's
    /// `NORBELYS_ROLE_KEY`.
    DeploymentKey {
        /// The role to print a role key for.
        #[arg(long = "for", value_enum)]
        role: Option<KeyRole>,
    },
    /// Prints the OpenAPI document of `/v1`, derived from the handlers, as stable text (sorted
    /// keys, a final newline).
    Openapi {
        /// Only the public surface: without the dashboard operations and the schemas only they
        /// use. This is what `crates/server/openapi.json` holds.
        #[arg(long)]
        public: bool,
        /// Write the document to this file instead of standard output (a task runner without a
        /// shell cannot redirect, and no log line can reach a file written this way).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Creates a workspace with its owner and a first API key, printed once as JSON.
    CreateWorkspace {
        /// The workspace's slug: lowercase letters, digits and hyphens.
        #[arg(long)]
        slug: String,
        /// The workspace's display name.
        #[arg(long)]
        name: String,
        /// The owner's email address; an existing user with it becomes the owner.
        #[arg(long)]
        owner_email: String,
        /// Create a test-mode workspace (a fake transport; `nb_test_` keys).
        #[arg(long)]
        test: bool,
    },
    /// Manages the keys that sign workspace tokens.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Secrets at rest: re-sealing them under a new deployment key.
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// Connections: an operator's resync of a mailbox.
    Connections {
        #[command(subcommand)]
        command: ConnectionsCommand,
    },
    /// Makes an operator an owner of the `system` workspace and prints a first API key once, as
    /// JSON. The system workspace's connections (the relay that sends sign-in codes and
    /// invitations) are then managed through the API like any workspace's; its identity tagged
    /// `transactional` sends the platform's transactional mail.
    SystemApiKey {
        /// The operator's email address; an existing user with it becomes an owner.
        #[arg(long)]
        owner_email: String,
    },
    /// Prepares one message for submission exactly as the sender does, and prints its envelope
    /// and MIME. Sends nothing and writes nothing.
    RenderMessage {
        /// Shared object storage for attachments.
        #[command(flatten)]
        storage: Box<StorageArgs>,
        /// The message's workspace (`ws_…`).
        #[arg(long)]
        workspace: String,
        /// The message (`msg_…`).
        #[arg(long)]
        message: String,
        #[command(flatten)]
        rendering: RenderingArgs,
        /// The sender's retry window, in hours, as the sender reads it: a Mailgun message without
        /// a deadline asks to be delivered within it.
        #[arg(long, env = "DELIVERY_RETRY_WINDOW_HOURS", default_value_t = 24)]
        retry_window_hours: u64,
    },
    /// Counter maintenance.
    Analytics {
        #[command(subcommand)]
        command: AnalyticsCommand,
    },
    /// Records a completed restore drill: the latest backup restored onto an isolated host, the
    /// schema checked and the row counts compared, in `--minutes` from the start of the restore.
    /// The worker reports the newest drill, and the `restore-drill` alert fires when none was
    /// recorded for over a month. Prints the record as JSON.
    RestoreDrill {
        /// How long the drill took, from the start of the restore to the counts compared.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        minutes: u32,
    },
    /// Owner recovery: break-glass sessions.
    Owners {
        #[command(subcommand)]
        command: OwnersCommand,
    },
    /// People's accounts: impersonation for support.
    Users {
        #[command(subcommand)]
        command: UsersCommand,
    },
    /// Seeds a development database with a demonstrable test-mode workspace, made through the
    /// API in process: its owner, an API key printed once, people with fields and groups, a
    /// segment, a sender and a three-step campaign with enrollments, started. Prints what it made
    /// as JSON; does nothing when a workspace with the slug exists.
    Seed {
        /// The workspace's slug.
        #[arg(long, default_value = "demo")]
        slug: String,
        /// The owner's email address; an existing user with it becomes the owner.
        #[arg(long, default_value = "owner@demo.example")]
        owner_email: String,
        #[command(flatten)]
        mail: Box<MailArgs>,
        #[command(flatten)]
        storage: Box<StorageArgs>,
        #[command(flatten)]
        identity: IdentityArgs,
        #[command(flatten)]
        rendering: RenderingArgs,
    },
}

/// `admin analytics`.
#[derive(Debug, Subcommand)]
pub enum AnalyticsCommand {
    /// Recomputes one UTC day's campaign counters from the facts still in the database and
    /// overwrites the day (days before yesterday only: the nightly recount owns yesterday).
    /// Prints the number of counter rows the day now has.
    Rebuild {
        /// The day, `YYYY-MM-DD`.
        day: String,
    },
}

/// `admin keys`.
#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Adds a new signing key and retires the current ones in 24 hours: the monthly rotation,
    /// and the first key of a new deployment. Prints the new key's id.
    Rotate,
}

/// `admin secrets`.
#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Re-seals every stored secret (connection credentials, provider webhook keys, SSO client
    /// secrets, webhook endpoint secrets, signing keys, live sign-in ceremonies) under the current
    /// `NORBELYS_DEPLOYMENT_KEY`, opening each with the key it names, the previous one being
    /// `NORBELYS_PREVIOUS_DEPLOYMENT_KEY`. Run it once every role holds the new key. Prints, per
    /// sealed column, the rows re-sealed, changed meanwhile and unopenable, and `done` once no
    /// stored secret is left under another key, as JSON.
    Rotate {
        /// Rows per transaction.
        #[arg(long, default_value_t = 100)]
        batch: u32,
    },
}

/// `admin connections`.
#[derive(Debug, Subcommand)]
pub enum ConnectionsCommand {
    /// Reads a connection's mailbox again from shortly before its last poll: its receive bindings
    /// forget their cursors and are due at once, the bounded resync a provider's cursor reset
    /// starts by itself. Recorded with the reason in the workspace's audit log; prints the
    /// bindings resynced, as JSON.
    Resync {
        /// The connection (`con_…`).
        connection: String,
        /// Why, for the workspace's audit log.
        #[arg(long)]
        reason: String,
    },
}

/// `admin owners`.
#[derive(Debug, Subcommand)]
pub enum OwnersCommand {
    /// Opens a break-glass session for an owner locked out of a workspace by its single sign-on:
    /// their newest live session (or `--session`) becomes, for 24 hours at most, a session that
    /// may repair the workspace's single sign-on and members and reach nothing else. Reads one of
    /// the owner's recovery codes, registered before the enforcement began, from standard input;
    /// records the reason in the workspace's audit log and tells its other owners by email.
    /// Prints the session as JSON.
    BreakGlass {
        /// The workspace: its slug or its id (`ws_…`).
        #[arg(long)]
        workspace: String,
        /// The owner's email address.
        #[arg(long)]
        email: String,
        /// The session to open it on (`ses_…`, from the owner's list of sessions); their newest
        /// live session when absent.
        #[arg(long)]
        session: Option<String>,
        /// Why, for the audit log and the other owners.
        #[arg(long)]
        reason: String,
    },
}

/// `admin users`.
#[derive(Debug, Subcommand)]
pub enum UsersCommand {
    /// Opens a 10-minute session as a person, for support: recorded with the reason in the
    /// audit log of every workspace where they are a member, and listed among their sessions.
    /// Prints its cookie and CSRF token once, as JSON.
    Impersonate {
        /// The person's email address.
        #[arg(long)]
        email: String,
        /// Why, for the audit log and the person's list of sessions.
        #[arg(long)]
        reason: String,
    },
    /// Suspends a person: no sign-in, and every session they hold revoked (by the
    /// `sessions.revoke_user` job). Recorded with the reason in their own log; prints the person
    /// and their status, as JSON.
    Suspend {
        /// The person's email address.
        #[arg(long)]
        email: String,
        /// Why, for the person's log.
        #[arg(long)]
        reason: String,
    },
    /// Lets a suspended person sign in again; the sessions their suspension revoked stay revoked.
    /// Recorded with the reason in their own log; prints the person and their status, as JSON.
    Reactivate {
        /// The person's email address.
        #[arg(long)]
        email: String,
        /// Why, for the person's log.
        #[arg(long)]
        reason: String,
    },
}

/// The families of variable names that belong to this program. A variable of one of them that no
/// role reads and `.env.example` does not document is a typo or a renamed setting, which would
/// otherwise be ignored without a word: it stops the role at start instead
/// ([`check_environment`]). Families that other software shares (`AWS_`, `OTEL_`, `HTTP_`,
/// `RUST_`, the AI providers' own) are left out, so their variables never stop a role.
const OWN_PREFIXES: &[&str] = &[
    "NORBELYS_",
    "API_",
    "SENDER_",
    "INBOX_",
    "TRACKING_",
    "INGRESS_",
    "DELIVERY_",
    "JOB_",
    "WEBHOOK_",
    "AI_",
    "MAIL_",
    "MTA_",
    "OAUTH_",
    "GOOGLE_OAUTH_",
    "MICROSOFT_OAUTH_",
    "DASHBOARD_",
    "WEBAUTHN_",
    "CAPTCHA_",
    "RECAPTCHA_",
    "IDENTITY_",
    "AUTHORITY_",
    "OBJECT_STORE_",
    "SYSTEM_DATABASE_",
];

/// Names of this program's families that the deployment's own tooling sets around it (the compose
/// file's interpolation) and no role reads.
const TOOLING: &[&str] = &["NORBELYS_ENV_DIR", "NORBELYS_REGISTRY"];

/// Why a role refuses the environment it starts in ([`check_environment`]).
#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    /// Universal SSRF exceptions belong only to isolated development.
    #[error("private-target development exceptions are refused outside development")]
    PrivateTargets,
    /// Variables of this program's families that nothing reads.
    #[error(
        "unknown variables, read by no role (a typo, or a renamed setting): {}",
        .0.join(", ")
    )]
    Unknown(Vec<String>),
    /// Variables another role reads, given to this one.
    #[error(
        "variables of other roles than `{role}` (each role's environment holds its own settings only): {}",
        .names.join(", ")
    )]
    Misplaced {
        /// The role refusing them.
        role: &'static str,
        /// The variables.
        names: Vec<String>,
    },
}

/// Refuses the environment `role` starts in, before it does anything (see
/// [`environment_problems`]).
///
/// The rule has two parts. Everywhere, a variable of this program's families ([`OWN_PREFIXES`])
/// that no role reads, `.env.example` does not document and the deployment's tooling does not set
/// is refused: a mistyped or renamed setting would otherwise be ignored. Outside `development`,
/// where each role has an environment file of its own, a variable that another role reads but
/// this one does not is refused as well, so a secret given to the wrong role (an identity secret
/// to the sender) is caught where it lands; the operator's `admin` runs from wherever the operator
/// is and is spared that part, and in development one `.env` serves every role.
///
/// # Errors
///
/// [`EnvironmentError`], naming the variables.
pub fn check_environment(role: &Role) -> Result<(), EnvironmentError> {
    if matches!(role, Role::Healthcheck(_)) {
        return Ok(());
    }
    let private = match role {
        Role::Api(args) => {
            args.mail.mail_allow_private_hosts || args.identity.identity_allow_private_issuers
        }
        Role::Sender(args) => args.mail.mail_allow_private_hosts,
        Role::Inbox(args) => args.mail.mail_allow_private_hosts,
        Role::Worker(args) => {
            args.mail.mail_allow_private_hosts
                || args.webhook_allow_private_targets
                || args.identity_allow_private_issuers
        }
        _ => false,
    };
    if role.common().environment != "development" && private {
        return Err(EnvironmentError::PrivateTargets);
    }
    let present: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .collect();
    let cli = Cli::command();
    let everyone = read_variables(&cli);
    let own = cli
        .find_subcommand(role.name())
        .map(read_variables)
        .unwrap_or_default();
    let strict = role.common().environment != "development" && !matches!(role, Role::Admin(_));
    let (unknown, misplaced) =
        environment_problems(&present, &own, &everyone, &documented(), strict);
    if !unknown.is_empty() {
        return Err(EnvironmentError::Unknown(unknown));
    }
    if !misplaced.is_empty() {
        return Err(EnvironmentError::Misplaced {
            role: role.name(),
            names: misplaced,
        });
    }
    Ok(())
}

/// Every variable an argument of `command`, or of its subcommands at any depth, reads.
fn read_variables(command: &clap::Command) -> BTreeSet<String> {
    command
        .get_arguments()
        .filter_map(clap::Arg::get_env)
        .map(|name| name.to_string_lossy().into_owned())
        .chain(command.get_subcommands().flat_map(read_variables))
        .collect()
}

/// The names `.env.example` documents (as `NAME=…` or `# NAME=…`): every role's, and those of
/// the repository's other programs (the command-line client, the managed MTA, the tests), which
/// may share a development `.env`.
fn documented() -> BTreeSet<String> {
    include_str!("../../../.env.example")
        .lines()
        .filter_map(|line| {
            let (name, _) = line.trim_start_matches(['#', ' ']).split_once('=')?;
            let shaped = !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
            shaped.then(|| name.to_owned())
        })
        .collect()
}

/// The variables of `present` that stop a role whose own variables are `own`, sorted: those of
/// this program's families that no role reads (`everyone`), nobody documented and no tooling sets
/// (the unknown, first); and, when `strict`, those another role reads but this one does not (the
/// misplaced, second).
fn environment_problems(
    present: &[String],
    own: &BTreeSet<String>,
    everyone: &BTreeSet<String>,
    documented: &BTreeSet<String>,
    strict: bool,
) -> (Vec<String>, Vec<String>) {
    let mut unknown: Vec<String> = present
        .iter()
        .filter(|name| {
            OWN_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
                && !everyone.contains(*name)
                && !documented.contains(*name)
                && !TOOLING.contains(&name.as_str())
        })
        .cloned()
        .collect();
    let mut misplaced: Vec<String> = present
        .iter()
        .filter(|name| strict && everyone.contains(*name) && !own.contains(*name))
        .cloned()
        .collect();
    unknown.sort();
    misplaced.sort();
    (unknown, misplaced)
}

/// Explicit disposable-cluster input for the database test harness. Reject ambient application
/// credentials and alternate connection parameters before creating roles or databases.
#[cfg(test)]
pub(crate) fn test_admin_url() -> Option<String> {
    if std::env::var("NORBELYS_TEST_DATABASE_DISPOSABLE").as_deref() != Ok("1") {
        return None;
    }
    let raw = std::env::var("TEST_DATABASE_URL").ok()?;
    let url = url::Url::parse(&raw).ok()?;
    let name = url.path().strip_prefix('/')?;
    let local = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    (matches!(url.scheme(), "postgres" | "postgresql")
        && local
        && !matches!(name, "postgres" | "template0" | "template1")
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && url.fragment().is_none()
        && url.query_pairs().all(|(key, _)| key == "sslmode"))
    .then_some(raw)
}

/// What the evaluation of the AI prompts against real models reads from the environment: the
/// worker's AI variables (keys, catalogue, models), and `AI_EVAL_WRITE_BASELINE`, which makes a
/// passing run record its measurement as the new baseline. The evaluation runs only on request.
///
/// # Errors
///
/// A variable does not read, as the worker would refuse it.
#[cfg(test)]
pub(crate) fn test_eval_settings() -> Result<(AiArgs, bool), clap::Error> {
    /// The evaluation's variables.
    #[derive(Debug, Parser)]
    struct Evaluation {
        #[command(flatten)]
        ai: AiArgs,
        /// Record a passing run as the new baseline.
        #[arg(long, env = "AI_EVAL_WRITE_BASELINE")]
        write_baseline: bool,
    }
    let _ = dotenvy::dotenv();
    Evaluation::try_parse_from(["evaluation"])
        .map(|evaluation| (evaluation.ai, evaluation.write_baseline))
}

/// What the smoke test of a real bucket reads from the environment: the roles' storage variables
/// (`OBJECT_STORE_URL` and the `AWS_*` settings), read exactly as the roles read them, so the test
/// reaches the bucket the deployment would. The smoke test runs only on request.
///
/// # Errors
///
/// A variable does not read, as a role would refuse it.
#[cfg(test)]
pub(crate) fn test_storage_args() -> Result<StorageArgs, clap::Error> {
    /// The smoke test's variables.
    #[derive(Debug, Parser)]
    struct Smoke {
        #[command(flatten)]
        storage: StorageArgs,
    }
    let _ = dotenvy::dotenv();
    Smoke::try_parse_from(["smoke"]).map(|smoke| smoke.storage)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Cli, Role};
    use crate::jobs::Queue;

    /// Public roles cannot inherit isolated-development SSRF exceptions, including preview
    /// environments. The same flags remain usable by an explicitly local development role.
    #[test]
    fn private_target_exceptions_stop_every_affected_public_role() {
        for (role, flag) in [
            ("api", "--mail-allow-private-hosts"),
            ("api", "--identity-allow-private-issuers"),
            ("sender", "--mail-allow-private-hosts"),
            ("inbox", "--mail-allow-private-hosts"),
            ("worker", "--mail-allow-private-hosts"),
            ("worker", "--webhook-allow-private-targets"),
            ("worker", "--identity-allow-private-issuers"),
        ] {
            for environment in ["production", "preview", "development"] {
                let cli = Cli::try_parse_from([
                    "norbelys-server",
                    role,
                    "--environment",
                    environment,
                    flag,
                ])
                .unwrap();
                let result = super::check_environment(&cli.role);
                if environment == "development" {
                    assert!(result.is_ok(), "{role} {flag}: {result:?}");
                } else {
                    assert!(
                        matches!(result, Err(super::EnvironmentError::PrivateTargets)),
                        "{role} {flag} {environment}: {result:?}"
                    );
                }
            }
        }
    }

    /// `JOB_PERMITS` reads comma-separated `queue=permits` pairs into typed overrides and refuses
    /// an unknown queue, a missing or zero count, so a typo stops the worker at start instead of
    /// being silently ignored.
    #[test]
    fn job_permits_are_typed_pairs() {
        let cli = Cli::try_parse_from([
            "norbelys-server",
            "worker",
            "--job-permits",
            "webhooks=32,maintenance=1",
        ])
        .unwrap();
        let Role::Worker(args) = cli.role else {
            panic!("the worker role was not parsed");
        };
        assert_eq!(
            args.job_permits,
            vec![(Queue::Webhooks, 32), (Queue::Maintenance, 1)]
        );
        for wrong in ["webhook=3", "webhooks=0", "webhooks", "webhooks=many"] {
            assert!(
                Cli::try_parse_from(["norbelys-server", "worker", "--job-permits", wrong]).is_err(),
                "`{wrong}` was accepted"
            );
        }
    }
}

/// The policy that `.env.example`, the one list of every variable, stays complete.
#[cfg(test)]
mod env_example {
    use clap::CommandFactory as _;

    /// Every variable an argument of `command`, or of its subcommands at any depth, reads.
    fn variables(command: &clap::Command) -> Vec<String> {
        command
            .get_arguments()
            .filter_map(clap::Arg::get_env)
            .map(|name| name.to_string_lossy().into_owned())
            .chain(command.get_subcommands().flat_map(variables))
            .collect()
    }

    /// Every variable of every role is in `.env.example` (as `NAME=…` or `# NAME=…`), so
    /// whoever configures a deployment finds each setting, its default and its meaning in one
    /// place; the failure names the variables to add.
    #[test]
    fn every_variable_is_in_env_example() {
        let example = include_str!("../../../.env.example");
        let documented = |name: &String| {
            example.lines().any(|line| {
                line.trim_start_matches(['#', ' '])
                    .starts_with(&format!("{name}="))
            })
        };
        let variables = variables(&super::Cli::command());
        // The walk must reach the roles' arguments, or this test would pass on nothing.
        assert!(variables.iter().any(|name| name == "SYSTEM_DATABASE_URL"));
        let mut missing: Vec<String> = variables
            .into_iter()
            .filter(|name| !documented(name))
            .collect();
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "add these variables to .env.example, each with its default and meaning: {missing:?}"
        );
    }
}

/// The rule that refuses unknown and misplaced variables at start.
#[cfg(test)]
mod environment {
    use std::collections::BTreeSet;

    use clap::CommandFactory as _;

    use super::{Cli, documented, environment_problems, read_variables};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The variables role `role` reads.
    fn of(role: &str) -> BTreeSet<String> {
        read_variables(Cli::command().find_subcommand(role).expect("a role"))
    }

    /// A variable of this program's families that no role reads and nothing documents (a typo
    /// such as `SENDER_PERMIT`) stops a role, in development too; other software's variables, the
    /// documented variables of the repository's other programs, the deployment tooling's and every
    /// role's own pass.
    #[test]
    fn unknown_variables_stop_a_role() {
        let everyone = read_variables(&Cli::command());
        let present = names(&[
            "SENDER_PERMIT",
            "SENDER_PERMITS",
            "HTTP_PROXY",
            "AWS_PROFILE",
            "RUST_LOG",
            "NORBELYS_API_KEY",
            "TEST_DATABASE_URL",
            "NORBELYS_ENV_DIR",
            "CAPTCHA_SECRET",
        ]);
        let (unknown, misplaced) =
            environment_problems(&present, &of("sender"), &everyone, &documented(), false);
        assert_eq!(unknown, names(&["SENDER_PERMIT"]));
        assert!(misplaced.is_empty(), "{misplaced:?}");
    }

    /// Outside development each role has an environment file of its own, so a variable another
    /// role reads but this one does not (the api's captcha secret, the worker's system login,
    /// given to the sender) stops it: a secret never sits unnoticed in a role with no use for it.
    #[test]
    fn other_roles_variables_stop_a_role_outside_development() {
        let everyone = read_variables(&Cli::command());
        let present = names(&[
            "CAPTCHA_SECRET",
            "SYSTEM_DATABASE_URL",
            "SENDER_PERMITS",
            "DATABASE_URL",
        ]);
        let (unknown, misplaced) =
            environment_problems(&present, &of("sender"), &everyone, &documented(), true);
        assert!(unknown.is_empty(), "{unknown:?}");
        assert_eq!(misplaced, names(&["CAPTCHA_SECRET", "SYSTEM_DATABASE_URL"]));
    }

    /// Every role accepts all of its own variables at once, outside development too: the walk
    /// reaches each role's arguments, those it shares through flattened groups included.
    #[test]
    fn every_role_accepts_its_own_variables() {
        let everyone = read_variables(&Cli::command());
        for role in [
            "api",
            "sender",
            "inbox",
            "worker",
            "tracking",
            "analytics",
            "admin",
        ] {
            let own = of(role);
            assert!(own.contains("DATABASE_URL"), "{role}");
            let present: Vec<String> = own.iter().cloned().collect();
            assert_eq!(
                environment_problems(&present, &own, &everyone, &documented(), true),
                (Vec::new(), Vec::new()),
                "{role}"
            );
        }
    }
}
