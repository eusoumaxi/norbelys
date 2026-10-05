//! The Sending resources of `/v1`: `connections`, `quota_scopes` and `sending_domains`.
//!
//! - `connections`: list, create, retrieve, update, delete (archive) and `verify`. A
//!   credential-based connection is created at once in `verifying` (`201`); a Google or
//!   Microsoft one answers `202` with `authorization.url`, sets the ceremony cookie, and is
//!   created by the callback once the browser consents. `paused` is a reversible switch; a new
//!   credential or new server settings verify again. `verify` checks the credential now, or, for
//!   an OAuth grant that is lost, answers with `authorization.url`. A member manages only the
//!   connections they created.
//! - `quota_scopes`: list, create, retrieve, update, delete; deleting one that SES connections
//!   name is `409 invalid_state`, naming them.
//! - `sending_domains`: list, create, retrieve, update, delete and `verify`.
//!
//! Reads need `connections:read`; writes need `connections:manage`. Every effectful `POST`
//! takes an `Idempotency-Key` (the idempotency middleware enforces it before these handlers
//! run). The API never waits on a provider: proofs run as jobs, and the response shows the
//! resource in its intermediate status.
//!
//! All three resources can be updated, so each carries `version` and answers it as `ETag`
//! wherever one is returned alone (creates, retrieves, updates, an archived connection, the
//! `verify` actions). An update takes an optional `If-Match`, checked under the row's lock before
//! anything is written (`http::versioning`).

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::bindings::{INBOX, MAX_FOLDERS};
use super::connections::{
    self, Authorization, Changes, ConnectionObject, Filters, ImapSecurity, ImapSettings,
    NewConnection, Security, SmtpChange, SmtpSettings, Verified,
};
use super::credentials::{ApiCredential, Credential};
use super::domains::{self, DomainObject, SendingDomainStatus};
use super::identities::IdentityInput;
use super::oauth::{self, Intent, Pending};
use super::scopes::{self, LimitChanges, Limits, QuotaScopeObject, WindowUnit};
use super::{Error, Settings};
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Connection, Id, QuotaScope, SendingDomain, WorkspaceId};
use crate::domain::scope::Scope;
use crate::domain::senders::{Provider, SendWindow, Status, WayIn, WebhookKey, new_interval};
use crate::http::AppState;
use crate::http::extract::{Json, Path, Query};
use crate::http::versioning::{IfMatch, Tagged};
use crate::identity::authority::Principal;
use crate::identity::ceremonies::CeremonyError;
use crate::jobs::{self, Queue};
use crate::pagination::{self, COUNT_CAP, Include, ListQuery, Order, Page, PageParams};
use crate::problem::{ApiResult, Problem};

impl From<Error> for Problem {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(what) => Self::not_found(what),
            Error::Invalid { pointer, detail } => Self::invalid_field(&pointer, "invalid", detail),
            Error::InvalidState(detail) => Self::invalid_state(detail),
            Error::Conflict(detail) => Self::conflict(detail),
            Error::Db(error) | Error::Ceremony(CeremonyError::Db(error)) => error.into(),
            Error::Crypto(error) => error.into(),
            Error::Credential(error) => Self::internal(&error),
            Error::Ceremony(error) => Self::internal(&error),
        }
    }
}

/// The routes of the three resources.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_connections, create_connection))
        .routes(routes!(
            retrieve_connection,
            update_connection,
            delete_connection
        ))
        .routes(routes!(verify_connection))
        .routes(routes!(list_quota_scopes, create_quota_scope))
        .routes(routes!(
            retrieve_quota_scope,
            update_quota_scope,
            delete_quota_scope
        ))
        .routes(routes!(list_sending_domains, create_sending_domain))
        .routes(routes!(
            retrieve_sending_domain,
            update_sending_domain,
            delete_sending_domain
        ))
        .routes(routes!(verify_sending_domain))
}

/// A field that may be given as `null` to clear it: absent is `None`, `null` is `Some(None)`.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The settings of the Sending area.
fn settings(state: &AppState) -> &Settings {
    &state.settings.senders
}

/// Wakes the maintenance queue after a commit that enqueued a proof.
async fn wake(state: &AppState) {
    jobs::wake(&state.db, Queue::Maintenance).await;
}

/// A checked IANA time zone, `UTC` when none is given.
fn timezone(zone: Option<String>) -> Result<Option<String>, Problem> {
    match zone {
        None => Ok(None),
        Some(zone) if jiff::tz::TimeZone::get(&zone).is_ok() => Ok(Some(zone)),
        Some(_) => Err(Problem::invalid_field(
            "/timezone",
            "format",
            "The time zone is an IANA name, such as `Europe/Madrid`.",
        )),
    }
}

/// A checked send window, its days in order.
fn window(window: SendWindow) -> Result<SendWindow, Problem> {
    SendWindow::parse(&window.days, &window.start, &window.end)
        .map_err(|error| Problem::invalid_field("/send_window", "invalid", error.to_string()))
}

/// A relay's API credential from the body, its lengths checked (its fit to the provider is
/// checked by the update, which knows the provider).
fn api_credential(input: ApiCredentialInput) -> Result<ApiCredential, Problem> {
    if input
        .id
        .as_ref()
        .is_some_and(|id| !(16..=128).contains(&id.len()))
    {
        return Err(Problem::invalid_field(
            "/api_credential/id",
            "length",
            "An AWS access key id has 16 to 128 characters.",
        ));
    }
    if !(1..=4_096).contains(&input.secret.len()) {
        return Err(Problem::invalid_field(
            "/api_credential/secret",
            "length",
            "The API credential's secret has 1 to 4096 characters.",
        ));
    }
    Ok(ApiCredential {
        id: input.id,
        secret: SecretString::from(input.secret),
    })
}

/// A checked warm-up stage.
fn warmup(stage: Option<i16>) -> Result<Option<i16>, Problem> {
    match stage {
        Some(stage) if !(0..=100).contains(&stage) => Err(Problem::invalid_field(
            "/warmup_stage",
            "range",
            "The warm-up stage is between 0 and 100.",
        )),
        stage => Ok(stage),
    }
}

/// Checked folder names.
fn folders(names: Vec<String>) -> Result<Vec<String>, Problem> {
    if names.len() > MAX_FOLDERS {
        return Err(Problem::invalid_field(
            "/receiving/folders",
            "length",
            "A connection reads at most 10 folders.",
        ));
    }
    for (index, name) in names.iter().enumerate() {
        if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
            return Err(Problem::invalid_field(
                &format!("/receiving/folders/{index}"),
                "invalid",
                "A folder name is 1 to 255 characters without control characters.",
            ));
        }
    }
    Ok(names)
}

/// A checked relative return path: `/` and more, never another origin.
fn return_path(path: Option<String>) -> Result<String, Problem> {
    let path = path.unwrap_or_else(|| "/".to_owned());
    let relative = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && path.len() <= 512
        && !path.chars().any(char::is_control);
    if relative {
        Ok(path)
    } else {
        Err(Problem::invalid_field(
            "/return_to",
            "format",
            "The return path is relative to the dashboard: it starts with one `/`.",
        ))
    }
}

/// `response`, which names a consent the browser must give, with the ceremony cookie that binds
/// the consent to this browser.
fn consent(started: &oauth::Started, mut response: Response) -> ApiResult<Response> {
    let cookie = HeaderValue::from_str(&started.cookie)
        .map_err(|_| Problem::internal(&"the ceremony cookie is not a header value"))?;
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

// ───────────────────────────── connections ─────────────────────────────

/// The filters of `GET /connections`.
#[derive(Debug, Deserialize)]
struct ConnectionQuery {
    status: Option<Status>,
    provider: Option<Provider>,
    tag: Option<String>,
    quota_scope_id: Option<Id<QuotaScope>>,
    q: Option<String>,
}

/// List the workspace's connections, newest first by default.
#[utoipa::path(
    get,
    path = "/connections",
    tag = "Sending",
    operation_id = "connections.list",
    params(
        ("limit" = Option<i64>, Query, description = "Connections per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<Status>, Query, description = "Only connections in this status."),
        ("provider" = Option<Provider>, Query, description = "Only connections of this provider."),
        ("tag" = Option<String>, Query, description = "Only connections with an identity carrying this tag."),
        ("quota_scope_id" = Option<Id<QuotaScope>>, Query, description = "Only connections naming this quota scope."),
        ("q" = Option<String>, Query, description = "Only connections whose account starts with this text."),
    ),
    responses(
        (status = 200, description = "A page of connections.", body = Page<ConnectionObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_connections(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<ConnectionQuery>,
) -> ApiResult<Json<Page<ConnectionObject>>> {
    principal.require(Scope::ConnectionsRead)?;
    let filters = Filters {
        status: query.status.map(|status| status.as_str().to_owned()),
        provider: query.provider.map(|provider| provider.as_str().to_owned()),
        tag: query.tag,
        quota_scope_id: query.quota_scope_id.map(|scope| scope.uuid()),
        q: query.q,
    };
    let params = PageParams::from_query(
        &state.keys,
        principal.workspace,
        "connections",
        "id",
        &filters,
        &list,
    )?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let rows = connections::list(
        &mut tx,
        settings(&state),
        principal.workspace,
        &filters,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(connections::count(&mut tx, principal.workspace, &filters, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&state.keys, &params, rows, |connection| {
        pagination::by_id(connection.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// An SMTP endpoint and its login, as a request gives it.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct SmtpInput {
    /// The host name.
    #[garde(length(min = 1, max = 253))]
    host: String,
    /// The port.
    #[garde(range(min = 1))]
    port: u16,
    /// `tls` (465), `starttls` (587), or `plain` (a private host in development only).
    #[garde(skip)]
    security: Security,
    /// The login; an SMTP login's is its account (`account_email`).
    #[garde(length(min = 1, max = 254))]
    username: Option<String>,
    /// The password, app password or relay key.
    #[garde(length(min = 1, max = 4096))]
    password: String,
    /// Amazon SES: the configuration set every message names.
    #[garde(pattern(r"^[A-Za-z0-9_-]{1,64}$"))]
    configuration_set: Option<String>,
}

/// An IMAP endpoint, as a request gives it; it logs in with the SMTP login and password.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ImapInput {
    /// The host name.
    #[garde(length(min = 1, max = 253))]
    host: String,
    /// The port.
    #[garde(range(min = 1))]
    port: u16,
    /// `tls` (993), or `plain` (a private host in development only).
    #[garde(skip)]
    security: ImapSecurity,
}

impl From<ImapInput> for ImapSettings {
    fn from(input: ImapInput) -> Self {
        Self {
            host: input.host,
            port: input.port,
            security: input.security,
        }
    }
}

/// The folders to read.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ReceivingInput {
    /// Folder names at the provider, at most 10; `INBOX` is the inbox.
    #[garde(skip)]
    folders: Vec<String>,
}

/// A relay's webhook verification material.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct WebhookInput {
    /// Mailgun: the HTTP webhook signing key. SendGrid: the verification key its webhook shows
    /// (base64). SES: the ARN of the SNS topic the configuration set posts to.
    #[garde(length(min = 1, max = 4096))]
    key: String,
}

/// A relay's API credential, used besides its SMTP credential for the daily account checks
/// (Amazon SES: the account's sending state and its identities' verification) and to reconcile
/// its events (SendGrid's Email Activity, Mailgun's events). It is stored sealed and never shown.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ApiCredentialInput {
    /// Amazon SES: the AWS access key id (`AKIA…`); absent for SendGrid and Mailgun.
    #[garde(length(min = 16, max = 128))]
    id: Option<String>,
    /// The AWS secret access key, or the SendGrid or Mailgun API key.
    #[garde(length(min = 1, max = 4096))]
    secret: String,
}

/// The body of `POST /connections`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateConnection {
    /// `smtp`, `google`, `microsoft`, `ses`, `sendgrid`, `mailgun` or `norbelys`.
    #[garde(skip)]
    provider: Provider,
    /// The account (not for Google and Microsoft, whose address the provider names): an SMTP
    /// login, the From address of a paced SES connection or of the managed MTA, a name for a
    /// relay account.
    #[garde(length(chars, min = 1, max = 254))]
    account_email: Option<String>,
    /// The SMTP endpoint and credential (SMTP logins and relays).
    #[garde(dive)]
    smtp: Option<SmtpInput>,
    /// The IMAP endpoint of an SMTP login that is read.
    #[garde(dive)]
    imap: Option<ImapInput>,
    /// The From addresses, at most 50; by default the account's own address.
    #[garde(length(max = 50), dive)]
    identities: Option<Vec<IdentityInput>>,
    /// The folders to read (mailboxes); `INBOX` by default.
    #[garde(dive)]
    receiving: Option<ReceivingInput>,
    /// A relay's webhook verification material; may be given later.
    #[garde(dive)]
    webhook: Option<WebhookInput>,
    /// Submissions a UTC day.
    #[garde(range(min = 1, max = 1_000_000))]
    daily_limit: Option<i32>,
    /// A paced sender's minutes between cold sends, 5 to 1,440, rounded up to whole 5-minute
    /// slots; 10 for a mailbox when absent; on SES it makes the connection a paced sender of one
    /// From address; refused for SendGrid, Mailgun and the managed MTA.
    #[garde(skip)]
    send_interval_minutes: Option<i32>,
    /// When campaign mail may be submitted.
    #[garde(skip)]
    send_window: Option<SendWindow>,
    /// The send window's IANA time zone (`UTC` by default).
    #[garde(length(min = 1, max = 64))]
    timezone: Option<String>,
    /// The warm-up stage to start at; absent when not warming.
    #[garde(skip)]
    warmup_stage: Option<i16>,
    /// The quota scope of the account; required for SES.
    #[garde(skip)]
    #[schema(value_type = Option<String>)]
    quota_scope_id: Option<Id<QuotaScope>>,
    /// Google and Microsoft: the dashboard path the browser returns to after the consent.
    #[garde(skip)]
    return_to: Option<String>,
}

/// The `202` of a consent: open `authorization.url` in the browser that holds the ceremony
/// cookie of this answer.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ConsentAnswer {
    authorization: Authorization,
}

/// Refuses a field the provider does not take.
fn absent<T>(field: &Option<T>, pointer: &str, why: &str) -> Result<(), Problem> {
    match field {
        Some(_) => Err(Problem::invalid_field(pointer, "invalid", why)),
        None => Ok(()),
    }
}

/// Create a connection.
///
/// A credential-based one (SMTP, a relay, the managed MTA) is created in `verifying`, or the
/// workspace's archived connection of the same account comes back with its id, history and
/// identities; a check (or the managed MTA's provisioning) proves it. A Google or Microsoft mailbox
/// answers `202` with the consent URL; the callback creates it.
#[utoipa::path(
    post,
    path = "/connections",
    tag = "Sending",
    operation_id = "connections.create",
    request_body(content = CreateConnection, example = json!({
        "provider": "smtp", "account_email": "ada@example.com",
        "smtp": {"host": "smtp.example.com", "port": 587, "security": "starttls", "password": "app-password"},
        "imap": {"host": "imap.example.com", "port": 993, "security": "tls"},
        "daily_limit": 50, "send_interval_minutes": 10
    })),
    responses(
        (status = 201, description = "The connection, `verifying`.", body = ConnectionObject,
         headers(("ETag" = String, description = "The connection's `version`, quoted."))),
        (status = 202, description = "Google or Microsoft: the consent to open, with the ceremony cookie.", body = ConsentAnswer),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 404, description = "No such quota scope in this workspace."),
        (status = 409, description = "The account is connected already (`conflict`), naming the live connection."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_connection(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateConnection>,
) -> ApiResult<Response> {
    principal.require(Scope::ConnectionsManage)?;
    let provider = body.provider;
    let interval = new_interval(provider, body.send_interval_minutes).map_err(|error| {
        Problem::invalid_field("/send_interval_minutes", "invalid", error.to_string())
    })?;
    let send_window = body.send_window.map(window).transpose()?;
    let zone = timezone(body.timezone)?.unwrap_or_else(|| "UTC".to_owned());
    let warmup_stage = warmup(body.warmup_stage)?;
    let daily_limit = body
        .daily_limit
        .unwrap_or_else(|| provider.default_daily_limit());
    let requested_folders = body
        .receiving
        .map(|receiving| folders(receiving.folders))
        .transpose()?;
    let identities = body.identities.unwrap_or_default();

    if provider.way_in() == WayIn::OAuth {
        let oauth_only = "Google and Microsoft name the account themselves.";
        absent(&body.account_email, "/account_email", oauth_only)?;
        absent(&body.smtp, "/smtp", oauth_only)?;
        absent(&body.imap, "/imap", oauth_only)?;
        absent(
            &body.webhook,
            "/webhook",
            "A mailbox has no provider webhook.",
        )?;
        let return_to = return_path(body.return_to)?;
        let pending = Pending {
            identities,
            folders: requested_folders.unwrap_or_else(|| vec![INBOX.to_owned()]),
            daily_limit,
            send_interval_minutes: interval,
            send_window,
            timezone: zone,
            warmup_stage,
            quota_scope: body.quota_scope_id,
        };
        let mut tx = state.db.begin_in(principal.workspace).await?;
        connections::check_scope(&mut tx, principal.workspace, provider, pending.quota_scope)
            .await?;
        let started = oauth::start(
            &mut tx,
            &state.keys,
            settings(&state),
            principal.workspace,
            principal.actor.user(),
            provider,
            Intent::Connect {
                pending: Box::new(pending),
            },
            return_to,
            None,
        )
        .await?;
        tx.commit().await?;
        let body = ConsentAnswer {
            authorization: started.authorization.clone(),
        };
        return consent(
            &started,
            (StatusCode::ACCEPTED, axum::Json(body)).into_response(),
        );
    }

    absent(
        &body.return_to,
        "/return_to",
        "Only an OAuth consent returns to a page.",
    )?;
    let account_email = body.account_email.ok_or_else(|| {
        Problem::invalid_field("/account_email", "required", "The account is required.")
    })?;
    let address = EmailAddress::parse(&account_email).ok();
    let (smtp, credential) = match (provider, body.smtp) {
        (Provider::Norbelys, Some(_)) => {
            return Err(Problem::invalid_field(
                "/smtp",
                "invalid",
                "The managed MTA's login is provisioned, not given.",
            ));
        }
        (Provider::Norbelys, None) => {
            let Some(address) = &address else {
                return Err(Problem::invalid_field(
                    "/account_email",
                    "format",
                    "A managed MTA login is an address on your verified domain.",
                ));
            };
            let smtp = SmtpSettings {
                host: settings(&state).mta_submission_host.clone(),
                port: 587,
                security: Security::Starttls,
                username: address.key(),
                configuration_set: None,
            };
            (smtp, None)
        }
        (_, None) => {
            return Err(Problem::invalid_field(
                "/smtp",
                "required",
                "The SMTP endpoint and its credential are required.",
            ));
        }
        (_, Some(input)) => {
            let username = match (provider, input.username) {
                (Provider::Smtp, Some(username))
                    if !username.eq_ignore_ascii_case(&account_email) =>
                {
                    return Err(Problem::invalid_field(
                        "/smtp/username",
                        "invalid",
                        "An SMTP login's account is its login: leave `username` out or equal to `account_email`.",
                    ));
                }
                (Provider::Smtp, _) => account_email.clone(),
                (_, Some(username)) => username,
                (_, None) => {
                    return Err(Problem::invalid_field(
                        "/smtp/username",
                        "required",
                        "A relay's SMTP login is required.",
                    ));
                }
            };
            match (provider, &input.configuration_set) {
                (Provider::Ses, None) => {
                    return Err(Problem::invalid_field(
                        "/smtp/configuration_set",
                        "required",
                        "An SES connection names its configuration set, so SES publishes its events.",
                    ));
                }
                (Provider::Ses, Some(_)) | (_, None) => {}
                (_, Some(_)) => {
                    return Err(Problem::invalid_field(
                        "/smtp/configuration_set",
                        "invalid",
                        "Only an SES connection names a configuration set.",
                    ));
                }
            }
            let smtp = SmtpSettings {
                host: input.host,
                port: input.port,
                security: input.security,
                username,
                configuration_set: input.configuration_set,
            };
            (
                smtp,
                Some(Credential::Password(SecretString::from(input.password))),
            )
        }
    };
    if provider != Provider::Smtp {
        absent(&body.imap, "/imap", "Only an SMTP login is read over IMAP.")?;
    }
    if provider == Provider::Ses && interval.is_some() && address.is_none() {
        return Err(Problem::invalid_field(
            "/account_email",
            "format",
            "A paced SES connection's account is the From address it paces.",
        ));
    }
    let webhook_key = match (provider.webhook_key(), body.webhook) {
        (None | Some(WebhookKey::Generated), Some(_)) => {
            return Err(Problem::invalid_field(
                "/webhook",
                "invalid",
                "Only a relay's webhook takes the provider's key.",
            ));
        }
        (Some(kind), Some(webhook)) => {
            let key = SecretString::from(webhook.key);
            connections::check_webhook_key(kind, &key)?;
            Some(key)
        }
        (_, None) => None,
    };
    let reads = provider == Provider::Smtp && body.imap.is_some();
    let folders = match requested_folders {
        Some(folders) if !reads && !folders.is_empty() => {
            return Err(Problem::invalid_field(
                "/receiving/folders",
                "invalid",
                "Only a mailbox is read: an SMTP login with IMAP settings.",
            ));
        }
        Some(folders) => folders,
        None if reads => vec![INBOX.to_owned()],
        None => Vec::new(),
    };
    let identities = match (identities.is_empty(), &address) {
        (true, Some(address)) => vec![IdentityInput::address(address.clone(), false)],
        _ => identities,
    };
    connections::check_paced_relay_identities(provider, interval, &account_email, &identities)?;
    let new = NewConnection {
        provider,
        account_email,
        subject: None,
        smtp: Some(smtp),
        imap: body.imap.map(ImapSettings::from),
        credential,
        identities,
        folders,
        webhook_key,
        daily_limit,
        send_interval_minutes: interval,
        send_window,
        timezone: zone,
        warmup_stage,
        quota_scope: body.quota_scope_id,
        created_by: principal.actor.user(),
    };
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let landed = connections::connect(&mut tx, &state.keys, principal.workspace, &new).await?;
    let connection = connections::read(&mut tx, settings(&state), principal.workspace, landed.id())
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    tx.commit().await?;
    wake(&state).await;
    let created = Tagged {
        version: connection.version,
        body: connection,
    };
    Ok((StatusCode::CREATED, created).into_response())
}

/// Retrieve a connection.
#[utoipa::path(
    get,
    path = "/connections/{id}",
    tag = "Sending",
    operation_id = "connections.retrieve",
    params(("id" = Id<Connection>, Path, description = "The connection id (`con_…`).")),
    responses(
        (status = 200, description = "The connection.", body = ConnectionObject,
         headers(("ETag" = String, description = "The connection's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:read`."),
        (status = 404, description = "No such connection in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_connection(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Connection>>,
) -> ApiResult<Tagged<ConnectionObject>> {
    principal.require(Scope::ConnectionsRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let connection = connections::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: connection.version,
        body: connection,
    })
}

/// A change of an SMTP endpoint: the fields given replace the stored ones.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct SmtpPatch {
    #[garde(length(min = 1, max = 253))]
    host: Option<String>,
    #[garde(range(min = 1))]
    port: Option<u16>,
    #[garde(skip)]
    security: Option<Security>,
    #[garde(length(min = 1, max = 254))]
    username: Option<String>,
    /// A new password: the connection is verified again.
    #[garde(length(min = 1, max = 4096))]
    password: Option<String>,
    #[garde(pattern(r"^[A-Za-z0-9_-]{1,64}$"))]
    configuration_set: Option<String>,
}

/// The body of `PATCH /connections/{id}`: every field is optional; `null` clears a nullable one.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateConnection {
    /// Pause or resume sending.
    #[garde(skip)]
    paused: Option<bool>,
    #[garde(range(min = 1, max = 1_000_000))]
    daily_limit: Option<i32>,
    /// A paced sender's new interval, rounded up to whole 5-minute slots; a rate-paced
    /// connection takes none.
    #[garde(skip)]
    send_interval_minutes: Option<i32>,
    #[garde(skip)]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<SendWindow>)]
    send_window: Option<Option<SendWindow>>,
    #[garde(length(min = 1, max = 64))]
    timezone: Option<String>,
    #[garde(skip)]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i16>)]
    warmup_stage: Option<Option<i16>>,
    /// The whole list of identities: one with its `id` is replaced, one without is added, one
    /// left out is removed (one with history is kept: disable it instead).
    #[garde(length(max = 50), dive)]
    identities: Option<Vec<IdentityInput>>,
    /// The whole list of folders to read.
    #[garde(dive)]
    receiving: Option<ReceivingInput>,
    /// New SMTP settings or credential: the connection is verified again.
    #[garde(dive)]
    smtp: Option<SmtpPatch>,
    /// New IMAP settings, or `null` to stop reading over IMAP: verified again.
    #[garde(skip)]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<ImapInput>)]
    imap: Option<Option<ImapInput>>,
    /// Another quota scope, or `null` (not for SES).
    #[garde(skip)]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    quota_scope_id: Option<Option<Id<QuotaScope>>>,
    /// A relay's webhook verification material.
    #[garde(dive)]
    webhook: Option<WebhookInput>,
    /// A relay's API credential (SES, SendGrid, Mailgun), or `null` to remove it: the
    /// connection is verified again.
    #[garde(skip)]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<ApiCredentialInput>)]
    api_credential: Option<Option<ApiCredentialInput>>,
}

/// Reads the connection's owner and applies the member-own rule.
async fn may_manage(
    principal: &Principal,
    tx: &mut crate::db::Tx,
    workspace: WorkspaceId,
    id: Id<Connection>,
) -> ApiResult<()> {
    let owner = connections::owner(tx, workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    principal.require_owned(Scope::ConnectionsManage, owner)
}

/// Update a connection.
///
/// Pause or resume it, change its pacing, identities or folders, or give it a new credential (then
/// it is verified again).
#[utoipa::path(
    patch,
    path = "/connections/{id}",
    tag = "Sending",
    operation_id = "connections.update",
    params(("id" = Id<Connection>, Path, description = "The connection id (`con_…`)."), IfMatch),
    request_body(content = UpdateConnection, example = json!({"paused": true})),
    responses(
        (status = 200, description = "The connection.", body = ConnectionObject,
         headers(("ETag" = String, description = "The connection's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`, or a member does not own the connection."),
        (status = 404, description = "No such connection in this workspace."),
        (status = 409, description = "The connection is archived (`invalid_state`), or an identity's address belongs to another connection (`conflict`)."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_connection(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Connection>>,
    if_match: IfMatch,
    Json(body): Json<UpdateConnection>,
) -> ApiResult<Tagged<ConnectionObject>> {
    principal.require(Scope::ConnectionsManage)?;
    let changes = Changes {
        paused: body.paused,
        daily_limit: body.daily_limit,
        send_interval_minutes: body.send_interval_minutes,
        send_window: body
            .send_window
            .map(|window_change| window_change.map(window).transpose())
            .transpose()?,
        timezone: timezone(body.timezone)?,
        warmup_stage: body.warmup_stage.map(warmup).transpose()?,
        identities: body.identities,
        folders: body
            .receiving
            .map(|receiving| folders(receiving.folders))
            .transpose()?,
        smtp: body.smtp.map(|smtp| SmtpChange {
            host: smtp.host,
            port: smtp.port,
            security: smtp.security,
            username: smtp.username,
            password: smtp.password.map(SecretString::from),
            configuration_set: smtp.configuration_set,
        }),
        imap: body.imap.map(|imap| imap.map(ImapSettings::from)),
        quota_scope: body.quota_scope_id,
        webhook_key: body.webhook.map(|webhook| SecretString::from(webhook.key)),
        api_credential: body
            .api_credential
            .map(|input| input.map(api_credential).transpose())
            .transpose()?,
    };
    let mut tx = state.db.begin_in(principal.workspace).await?;
    may_manage(&principal, &mut tx, principal.workspace, id).await?;
    let current = connections::lock_version(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    if_match.check(current)?;
    connections::update(&mut tx, &state.keys, principal.workspace, id, changes).await?;
    let connection = connections::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    tx.commit().await?;
    wake(&state).await;
    Ok(Tagged {
        version: connection.version,
        body: connection,
    })
}

/// Archive a connection.
///
/// It stops sending, its credential is erased and its folders are no longer read, while its history
/// stays readable and its provider webhook keeps receiving late evidence. Connecting the same
/// account again restores it.
#[utoipa::path(
    delete,
    path = "/connections/{id}",
    tag = "Sending",
    operation_id = "connections.delete",
    params(("id" = Id<Connection>, Path, description = "The connection id (`con_…`).")),
    responses(
        (status = 200, description = "The archived connection.", body = ConnectionObject,
         headers(("ETag" = String, description = "The connection's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`, or a member does not own the connection."),
        (status = 404, description = "No such connection in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_connection(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Connection>>,
) -> ApiResult<Tagged<ConnectionObject>> {
    principal.require(Scope::ConnectionsManage)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    may_manage(&principal, &mut tx, principal.workspace, id).await?;
    connections::archive(&mut tx, principal.workspace, id).await?;
    let connection = connections::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: connection.version,
        body: connection,
    })
}

/// The query of `POST /connections/{id}/verify`.
#[derive(Debug, Deserialize)]
struct VerifyQuery {
    /// Where the browser returns after a new consent.
    return_to: Option<String>,
}

/// Verify a connection now.
///
/// It becomes `verifying` and its check runs (the managed MTA's provisioning, for a `norbelys`
/// connection). For an OAuth connection whose grant is lost, the answer carries `authorization.url`
/// for the browser instead, and the ceremony cookie.
#[utoipa::path(
    post,
    path = "/connections/{id}/verify",
    tag = "Sending",
    operation_id = "connections.verify",
    params(
        ("id" = Id<Connection>, Path, description = "The connection id (`con_…`)."),
        ("return_to" = Option<String>, Query, description = "OAuth: the dashboard path the browser returns to after the consent."),
    ),
    responses(
        (status = 200, description = "The connection, `verifying`, or with `authorization` when consent is needed.", body = ConnectionObject,
         headers(("ETag" = String, description = "The connection's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`, or a member does not own the connection."),
        (status = 404, description = "No such connection in this workspace."),
        (status = 409, description = "The connection is archived (`invalid_state`)."),
        (status = 422, description = "The return path is invalid."),
    ),
    security(("bearer" = []))
)]
async fn verify_connection(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<Connection>>,
    Query(query): Query<VerifyQuery>,
) -> ApiResult<Response> {
    principal.require(Scope::ConnectionsManage)?;
    let return_to = return_path(query.return_to)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    may_manage(&principal, &mut tx, principal.workspace, id).await?;
    let verified = connections::verify(&mut tx, principal.workspace, id).await?;
    let started = match verified {
        Verified::Checking => None,
        Verified::Consent {
            provider,
            account_email,
        } => Some(
            oauth::start(
                &mut tx,
                &state.keys,
                settings(&state),
                principal.workspace,
                principal.actor.user(),
                provider,
                Intent::Reconnect { connection: id },
                return_to,
                Some(&account_email),
            )
            .await?,
        ),
    };
    let mut connection = connections::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("connection"))?;
    tx.commit().await?;
    match started {
        None => {
            wake(&state).await;
            Ok(Tagged {
                version: connection.version,
                body: connection,
            }
            .into_response())
        }
        Some(started) => {
            connection.authorization = Some(started.authorization.clone());
            let answer = Tagged {
                version: connection.version,
                body: connection,
            };
            consent(&started, answer.into_response())
        }
    }
}

// ───────────────────────────── quota scopes ─────────────────────────────

/// The filters of `GET /quota_scopes`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct ScopeQuery {
    provider: Option<Provider>,
}

/// List the workspace's quota scopes, newest first by default.
#[utoipa::path(
    get,
    path = "/quota_scopes",
    tag = "Sending",
    operation_id = "quota_scopes.list",
    params(
        ("limit" = Option<i64>, Query, description = "Scopes per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("provider" = Option<Provider>, Query, description = "Only scopes of this provider."),
    ),
    responses(
        (status = 200, description = "A page of quota scopes.", body = Page<QuotaScopeObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_quota_scopes(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<ScopeQuery>,
) -> ApiResult<Json<Page<QuotaScopeObject>>> {
    principal.require(Scope::ConnectionsRead)?;
    let params = PageParams::from_query(
        &state.keys,
        principal.workspace,
        "quota_scopes",
        "id",
        &query,
        &list,
    )?;
    let provider = query.provider.map(Provider::as_str);
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let rows = scopes::list(
        &mut tx,
        settings(&state),
        principal.workspace,
        provider,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(scopes::count(&mut tx, principal.workspace, provider, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&state.keys, &params, rows, |scope| {
        pagination::by_id(scope.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// The body of `POST /quota_scopes`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateQuotaScope {
    /// The provider of the account.
    #[garde(skip)]
    provider: Provider,
    /// The account: a project id, a tenant id, `account:region`, a relay host.
    #[garde(length(chars, min = 1, max = 255))]
    scope_key: String,
    #[garde(range(min = 1))]
    messages_per_day: Option<i32>,
    #[garde(range(min = 1))]
    recipients_per_day: Option<i32>,
    /// Units a short window allows; with `window_unit` and `window_seconds`, or none of them.
    #[garde(range(min = 1))]
    window_limit: Option<i32>,
    #[garde(skip)]
    window_unit: Option<WindowUnit>,
    #[garde(range(min = 1, max = 3600))]
    window_seconds: Option<i32>,
}

/// Create a quota scope.
#[utoipa::path(
    post,
    path = "/quota_scopes",
    tag = "Sending",
    operation_id = "quota_scopes.create",
    request_body(content = CreateQuotaScope, example = json!({
        "provider": "ses", "scope_key": "123456789012:eu-west-1", "messages_per_day": 50000,
        "window_limit": 14, "window_unit": "recipients", "window_seconds": 1
    })),
    responses(
        (status = 201, description = "The quota scope.", body = QuotaScopeObject,
         headers(("ETag" = String, description = "The quota scope's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 409, description = "The workspace has a scope of this provider and key (`conflict`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_quota_scope(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateQuotaScope>,
) -> ApiResult<(StatusCode, Tagged<QuotaScopeObject>)> {
    principal.require(Scope::ConnectionsManage)?;
    let window = [
        body.window_limit.is_some(),
        body.window_unit.is_some(),
        body.window_seconds.is_some(),
    ];
    if window.iter().any(|set| *set) && !window.iter().all(|set| *set) {
        return Err(Problem::invalid_field(
            "/window_limit",
            "invalid",
            "A short window is `window_limit`, `window_unit` and `window_seconds` together.",
        ));
    }
    let limits = Limits {
        messages_per_day: body.messages_per_day,
        recipients_per_day: body.recipients_per_day,
        window_limit: body.window_limit,
        window_unit: body.window_unit.map(|unit| unit.as_str().to_owned()),
        window_seconds: body.window_seconds,
    };
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let id = scopes::create(
        &mut tx,
        principal.workspace,
        body.provider.as_str(),
        &body.scope_key,
        &limits,
    )
    .await?;
    let scope = scopes::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("quota scope"))?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: scope.version,
            body: scope,
        },
    ))
}

/// Retrieve a quota scope.
#[utoipa::path(
    get,
    path = "/quota_scopes/{id}",
    tag = "Sending",
    operation_id = "quota_scopes.retrieve",
    params(("id" = Id<QuotaScope>, Path, description = "The quota scope id (`qsc_…`).")),
    responses(
        (status = 200, description = "The quota scope.", body = QuotaScopeObject,
         headers(("ETag" = String, description = "The quota scope's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:read`."),
        (status = 404, description = "No such quota scope in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_quota_scope(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<QuotaScope>>,
) -> ApiResult<Tagged<QuotaScopeObject>> {
    principal.require(Scope::ConnectionsRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let scope = scopes::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("quota scope"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: scope.version,
        body: scope,
    })
}

/// The body of `PATCH /quota_scopes/{id}`: a limit given replaces the stored one, `null` clears
/// it, an absent one stays; the short window's three fields change together.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateQuotaScope {
    #[garde(inner(range(min = 1)))]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>)]
    messages_per_day: Option<Option<i32>>,
    #[garde(inner(range(min = 1)))]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>)]
    recipients_per_day: Option<Option<i32>>,
    #[garde(inner(range(min = 1)))]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>)]
    window_limit: Option<Option<i32>>,
    #[garde(skip)]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<WindowUnit>)]
    window_unit: Option<Option<WindowUnit>>,
    #[garde(inner(range(min = 1, max = 3600)))]
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>)]
    window_seconds: Option<Option<i32>>,
}

/// Update a quota scope's limits: each limit given replaces the stored one, `null` clears it.
#[utoipa::path(
    patch,
    path = "/quota_scopes/{id}",
    tag = "Sending",
    operation_id = "quota_scopes.update",
    params(("id" = Id<QuotaScope>, Path, description = "The quota scope id (`qsc_…`)."), IfMatch),
    request_body(content = UpdateQuotaScope, example = json!({"messages_per_day": 100000, "window_limit": null, "window_unit": null, "window_seconds": null})),
    responses(
        (status = 200, description = "The quota scope.", body = QuotaScopeObject,
         headers(("ETag" = String, description = "The quota scope's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 404, description = "No such quota scope in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_quota_scope(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<QuotaScope>>,
    if_match: IfMatch,
    Json(body): Json<UpdateQuotaScope>,
) -> ApiResult<Tagged<QuotaScopeObject>> {
    principal.require(Scope::ConnectionsManage)?;
    let window = match (body.window_limit, body.window_unit, body.window_seconds) {
        (None, None, None) => None,
        (Some(None), Some(None), Some(None)) => Some(None),
        (Some(Some(limit)), Some(Some(unit)), Some(Some(seconds))) => {
            Some(Some((limit, unit.as_str().to_owned(), seconds)))
        }
        _ => {
            return Err(Problem::invalid_field(
                "/window_limit",
                "invalid",
                "A short window is `window_limit`, `window_unit` and `window_seconds` together.",
            ));
        }
    };
    let changes = LimitChanges {
        messages_per_day: body.messages_per_day,
        recipients_per_day: body.recipients_per_day,
        window,
    };
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let current = scopes::lock_version(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("quota scope"))?;
    if_match.check(current)?;
    if !scopes::update(&mut tx, principal.workspace, id, &changes).await? {
        return Err(Problem::not_found("quota scope"));
    }
    let scope = scopes::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("quota scope"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: scope.version,
        body: scope,
    })
}

/// Delete a quota scope and its ledger.
///
/// Its connections keep running without one. Refused while SES connections, archived ones included,
/// name it.
#[utoipa::path(
    delete,
    path = "/quota_scopes/{id}",
    tag = "Sending",
    operation_id = "quota_scopes.delete",
    params(("id" = Id<QuotaScope>, Path, description = "The quota scope id (`qsc_…`).")),
    responses(
        (status = 204, description = "The quota scope is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 404, description = "No such quota scope in this workspace."),
        (status = 409, description = "SES connections name it (`invalid_state`), named in `detail`."),
    ),
    security(("bearer" = []))
)]
async fn delete_quota_scope(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<QuotaScope>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::ConnectionsManage)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    scopes::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── sending domains ─────────────────────────────

/// The filters of `GET /sending_domains`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct DomainQuery {
    status: Option<SendingDomainStatus>,
}

/// List the workspace's sending domains, newest first by default.
#[utoipa::path(
    get,
    path = "/sending_domains",
    tag = "Sending",
    operation_id = "sending_domains.list",
    params(
        ("limit" = Option<i64>, Query, description = "Domains per page, 1 to 100 (default 20)."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("order" = Option<Order>, Query, description = "`desc` (default) or `asc`."),
        ("include" = Option<Include>, Query, description = "`total_count` adds an exact count up to 10,000."),
        ("status" = Option<SendingDomainStatus>, Query, description = "Only domains in this status."),
    ),
    responses(
        (status = 200, description = "A page of sending domains.", body = Page<DomainObject>),
        (status = 400, description = "The cursor is not valid for this request."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:read`."),
        (status = 422, description = "A parameter is invalid."),
    ),
    security(("bearer" = []))
)]
async fn list_sending_domains(
    principal: Principal,
    State(state): State<AppState>,
    Query(list): Query<ListQuery>,
    Query(query): Query<DomainQuery>,
) -> ApiResult<Json<Page<DomainObject>>> {
    principal.require(Scope::ConnectionsRead)?;
    let params = PageParams::from_query(
        &state.keys,
        principal.workspace,
        "sending_domains",
        "id",
        &query,
        &list,
    )?;
    let status = query.status.map(SendingDomainStatus::as_str);
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let rows = domains::list(
        &mut tx,
        settings(&state),
        principal.workspace,
        status,
        params.after_id(),
        params.ascending(),
        params.fetch(),
    )
    .await?;
    let total = match params.include_total {
        true => Some(domains::count(&mut tx, principal.workspace, status, COUNT_CAP).await?),
        false => None,
    };
    tx.commit().await?;
    let page = Page::new(&state.keys, &params, rows, |domain| {
        pagination::by_id(domain.id.uuid())
    });
    Ok(Json(match total {
        Some(total) => page.with_total(total),
        None => page,
    }))
}

/// The body of `POST /sending_domains`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateDomain {
    /// The hostname, such as `links.example.com`.
    #[garde(length(min = 1, max = 254))]
    hostname: String,
    /// Serve tracking links from it (its CNAME then points at Norbelys).
    #[garde(skip)]
    tracking_enabled: Option<bool>,
}

/// Create a sending domain, `pending_verification`, with the DNS records to publish.
#[utoipa::path(
    post,
    path = "/sending_domains",
    tag = "Sending",
    operation_id = "sending_domains.create",
    request_body(content = CreateDomain, example = json!({"hostname": "links.example.com", "tracking_enabled": true})),
    responses(
        (status = 201, description = "The domain with its records.", body = DomainObject,
         headers(("ETag" = String, description = "The domain's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 409, description = "The workspace has this hostname already (`conflict`)."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn create_sending_domain(
    principal: Principal,
    State(state): State<AppState>,
    Json(body): Json<CreateDomain>,
) -> ApiResult<(StatusCode, Tagged<DomainObject>)> {
    principal.require(Scope::ConnectionsManage)?;
    let hostname = domains::hostname(&body.hostname).ok_or_else(|| {
        Problem::invalid_field(
            "/hostname",
            "format",
            "A hostname is at least two labels of letters, digits and inner hyphens.",
        )
    })?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let id = domains::create(
        &mut tx,
        principal.workspace,
        &hostname,
        body.tracking_enabled.unwrap_or(false),
    )
    .await?;
    let domain = domains::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("sending domain"))?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Tagged {
            version: domain.version,
            body: domain,
        },
    ))
}

/// Retrieve a sending domain.
#[utoipa::path(
    get,
    path = "/sending_domains/{id}",
    tag = "Sending",
    operation_id = "sending_domains.retrieve",
    params(("id" = Id<SendingDomain>, Path, description = "The sending domain id (`dom_…`).")),
    responses(
        (status = 200, description = "The domain.", body = DomainObject,
         headers(("ETag" = String, description = "The domain's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:read`."),
        (status = 404, description = "No such sending domain in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn retrieve_sending_domain(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<SendingDomain>>,
) -> ApiResult<Tagged<DomainObject>> {
    principal.require(Scope::ConnectionsRead)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let domain = domains::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("sending domain"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: domain.version,
        body: domain,
    })
}

/// The body of `PATCH /sending_domains/{id}`.
#[derive(Debug, Deserialize, garde::Validate, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateDomain {
    /// Serve tracking links from it; `verify` then checks its CNAME.
    #[garde(skip)]
    tracking_enabled: bool,
}

/// Update a sending domain: turn tracking on or off.
#[utoipa::path(
    patch,
    path = "/sending_domains/{id}",
    tag = "Sending",
    operation_id = "sending_domains.update",
    params(("id" = Id<SendingDomain>, Path, description = "The sending domain id (`dom_…`)."), IfMatch),
    request_body(content = UpdateDomain, example = json!({"tracking_enabled": true})),
    responses(
        (status = 200, description = "The domain.", body = DomainObject,
         headers(("ETag" = String, description = "The domain's new `version`, quoted."))),
        (status = 400, description = "The body is not JSON, or `If-Match` is malformed (`invalid_request`)."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 404, description = "No such sending domain in this workspace."),
        (status = 412, description = "`If-Match` names a version that is no longer current (`precondition_failed`); nothing changed."),
        (status = 422, description = "The body is invalid."),
    ),
    security(("bearer" = []))
)]
async fn update_sending_domain(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<SendingDomain>>,
    if_match: IfMatch,
    Json(body): Json<UpdateDomain>,
) -> ApiResult<Tagged<DomainObject>> {
    principal.require(Scope::ConnectionsManage)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let current = domains::lock_version(&mut tx, principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("sending domain"))?;
    if_match.check(current)?;
    if !domains::update(&mut tx, principal.workspace, id, body.tracking_enabled).await? {
        return Err(Problem::not_found("sending domain"));
    }
    let domain = domains::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("sending domain"))?;
    tx.commit().await?;
    Ok(Tagged {
        version: domain.version,
        body: domain,
    })
}

/// Delete a sending domain.
#[utoipa::path(
    delete,
    path = "/sending_domains/{id}",
    tag = "Sending",
    operation_id = "sending_domains.delete",
    params(("id" = Id<SendingDomain>, Path, description = "The sending domain id (`dom_…`).")),
    responses(
        (status = 204, description = "The domain is deleted."),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 404, description = "No such sending domain in this workspace."),
    ),
    security(("bearer" = []))
)]
async fn delete_sending_domain(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<SendingDomain>>,
) -> ApiResult<StatusCode> {
    principal.require(Scope::ConnectionsManage)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    let deleted = domains::delete(&mut tx, principal.workspace, id).await?;
    tx.commit().await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("sending domain"))
    }
}

/// Verify a sending domain now: it becomes `verifying` and its DNS records are checked.
#[utoipa::path(
    post,
    path = "/sending_domains/{id}/verify",
    tag = "Sending",
    operation_id = "sending_domains.verify",
    params(("id" = Id<SendingDomain>, Path, description = "The sending domain id (`dom_…`).")),
    responses(
        (status = 200, description = "The domain, `verifying`.", body = DomainObject,
         headers(("ETag" = String, description = "The domain's `version`, quoted."))),
        (status = 401, description = "No valid credential."),
        (status = 403, description = "The credential lacks `connections:manage`."),
        (status = 404, description = "No such sending domain in this workspace."),
        (status = 409, description = "Another workspace holds the hostname (`conflict`)."),
    ),
    security(("bearer" = []))
)]
async fn verify_sending_domain(
    principal: Principal,
    State(state): State<AppState>,
    Path(id): Path<Id<SendingDomain>>,
) -> ApiResult<Tagged<DomainObject>> {
    principal.require(Scope::ConnectionsManage)?;
    let mut tx = state.db.begin_in(principal.workspace).await?;
    domains::verify(&mut tx, principal.workspace, id).await?;
    let domain = domains::read(&mut tx, settings(&state), principal.workspace, id)
        .await?
        .ok_or_else(|| Problem::not_found("sending domain"))?;
    tx.commit().await?;
    wake(&state).await;
    Ok(Tagged {
        version: domain.version,
        body: domain,
    })
}
