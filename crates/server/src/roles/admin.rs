//! The admin role: operator commands, run as `norbelys_system`.

mod connections;
mod secrets;
mod seed;
mod users;

use std::io::{IsTerminal as _, Write as _};
use std::time::Duration;

use anyhow::Context as _;

use crate::config::{
    AdminArgs, AdminCommand, AnalyticsCommand, ConnectionsCommand, KeysCommand, OwnersCommand,
    SecretsCommand, UsersCommand,
};
use crate::crypto;
use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Connection, Id, Message, Session, User, Workspace, WorkspaceId};
use crate::domain::scope::ScopeSet;
use crate::identity::api_keys::{self, KeyMode};
use crate::identity::audit::{self, Action, AuditActor};
use crate::identity::recovery;
use crate::identity::workspaces;

pub async fn run(args: AdminArgs) -> anyhow::Result<()> {
    match args.command {
        AdminCommand::SchemaCheck => {
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-schema-check", 1, Duration::from_secs(30)),
            )
            .await?;
            let mut connection = db.pool().acquire().await?;
            crate::db::schema::check(&mut connection).await?;
            writeln!(std::io::stdout(), "Database schema matches this release.")?;
            Ok(())
        }
        AdminCommand::DeploymentKey { role } => {
            let key = match role {
                None => crypto::new_deployment_key()?,
                Some(role) => crypto::role_key(
                    args.common
                        .deployment_key
                        .as_ref()
                        .context("a role key is made from NORBELYS_DEPLOYMENT_KEY: set it")?,
                    args.common.previous_deployment_key.as_ref(),
                    super::subkeys(role),
                )?,
            };
            writeln!(std::io::stdout(), "{key}")?;
            Ok(())
        }
        AdminCommand::Openapi { public, ref out } => {
            let document = crate::http::openapi::document(public)?;
            match out {
                Some(path) => std::fs::write(path, document)
                    .with_context(|| format!("cannot write {}", path.display()))?,
                // The text already ends in a newline.
                None => write!(std::io::stdout(), "{document}")?,
            }
            Ok(())
        }
        AdminCommand::CreateWorkspace {
            ref slug,
            ref name,
            ref owner_email,
            test,
        } => {
            let owner = EmailAddress::parse(owner_email)
                .context("--owner-email is not an email address")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let mut tx = db.begin().await?;
            let mode = if test { KeyMode::Test } else { KeyMode::Live };
            let created = workspaces::create_with_owner(&mut tx, slug, name, &owner, mode).await?;
            tx.commit().await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&created)?
            )?;
            Ok(())
        }
        AdminCommand::Keys {
            command: KeysCommand::Rotate,
        } => {
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let keys = super::keys(&args.common)?;
            let mut tx = db.begin().await?;
            let kid = crate::identity::tokens::rotate(&mut tx, &keys).await?;
            tx.commit().await?;
            writeln!(std::io::stdout(), "{kid}")?;
            Ok(())
        }
        AdminCommand::SystemApiKey { ref owner_email } => {
            let owner = EmailAddress::parse(owner_email)
                .context("--owner-email is not an email address")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let mut tx = db.begin().await?;
            let created = system_api_key(&mut tx, &owner).await?;
            tx.commit().await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&created)?
            )?;
            Ok(())
        }
        AdminCommand::RenderMessage {
            ref storage,
            ref workspace,
            ref message,
            ref rendering,
            retry_window_hours,
        } => {
            let workspace: Id<Workspace> = workspace
                .parse()
                .context("--workspace is not a workspace id (`ws_…`)")?;
            let message: Id<Message> = message
                .parse()
                .context("--message is not a message id (`msg_…`)")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let settings = crate::rendering::Settings::new(
                super::keys(&args.common)?,
                &rendering.tracking_url,
                Duration::from_secs(retry_window_hours.saturating_mul(3_600)),
            )?;
            let settings = settings.with_storage(crate::storage::Storage::from_args(
                storage,
                &args.common.environment,
            )?);
            let prepared = crate::rendering::prepare(
                &db,
                &settings,
                WorkspaceId::trusted(workspace.uuid()),
                message,
            )
            .await
            .map_err(|error| {
                let kind = if error.is_permanent() {
                    "can never be prepared"
                } else {
                    "could not be prepared now"
                };
                anyhow::anyhow!("the message {kind}: {error}")
            })?;
            let mut out = std::io::stdout();
            writeln!(out, "MAIL FROM:<{}>", prepared.envelope.from())?;
            for recipient in prepared.envelope.recipients() {
                writeln!(out, "RCPT TO:<{recipient}>")?;
            }
            writeln!(out)?;
            out.write_all(&prepared.raw)?;
            Ok(())
        }
        AdminCommand::Analytics {
            command: AnalyticsCommand::Rebuild { ref day },
        } => {
            let day = crate::domain::time::Date(
                day.parse()
                    .context("the day is not a date (`YYYY-MM-DD`)")?,
            );
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(300)),
            )
            .await?;
            let mut tx = db.begin().await?;
            let rows =
                crate::analytics::rollup::rebuild(&mut tx, day, crate::process::now()).await?;
            tx.commit().await?;
            writeln!(std::io::stdout(), "{rows}")?;
            Ok(())
        }
        AdminCommand::RestoreDrill { minutes } => {
            let minutes = i32::try_from(minutes).context("--minutes is too large")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let completed_at = sqlx::query_scalar!(
                r#"INSERT INTO restore_drills (minutes) VALUES ($1)
                   RETURNING completed_at AS "completed_at: crate::domain::time::Timestamp""#,
                minutes,
            )
            .fetch_one(db.pool())
            .await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "completed_at": completed_at, "minutes": minutes })
                )?
            )?;
            Ok(())
        }
        AdminCommand::Owners {
            command:
                OwnersCommand::BreakGlass {
                    ref workspace,
                    ref email,
                    ref session,
                    ref reason,
                },
        } => {
            let owner = EmailAddress::parse(email).context("--email is not an email address")?;
            let session = session
                .as_deref()
                .map(str::parse::<Id<Session>>)
                .transpose()
                .context("--session is not a session id (`ses_…`)")?;
            // The code comes from standard input, never the command line, which shells and
            // process lists keep.
            if std::io::stdin().is_terminal() {
                write!(std::io::stderr(), "One of the owner's recovery codes: ")?;
                std::io::stderr().flush()?;
            }
            let mut code = String::new();
            std::io::stdin().read_line(&mut code)?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let keys = super::keys(&args.common)?;
            let mut tx = db.begin().await?;
            let opened = recovery::break_glass(
                &mut tx,
                &keys,
                &recovery::BreakGlassRequest {
                    workspace,
                    owner: &owner,
                    code: code.trim(),
                    session,
                    reason,
                },
            )
            .await?;
            tx.commit().await?;
            crate::delivery::accept::wake(&db).await;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&opened)?
            )?;
            Ok(())
        }
        AdminCommand::Users {
            command:
                UsersCommand::Impersonate {
                    ref email,
                    ref reason,
                },
        } => {
            let person = EmailAddress::parse(email).context("--email is not an email address")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let keys = super::keys(&args.common)?;
            let mut tx = db.begin().await?;
            let opened = recovery::impersonate(&mut tx, &keys, &person, reason).await?;
            tx.commit().await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&opened)?
            )?;
            Ok(())
        }
        AdminCommand::Users {
            command:
                UsersCommand::Suspend {
                    ref email,
                    ref reason,
                },
        } => {
            let person = EmailAddress::parse(email).context("--email is not an email address")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let mut tx = db.begin().await?;
            let decided = users::suspend(&mut tx, &person, reason).await?;
            tx.commit().await?;
            crate::jobs::wake(&db, crate::jobs::Queue::Maintenance).await;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&decided)?
            )?;
            Ok(())
        }
        AdminCommand::Users {
            command:
                UsersCommand::Reactivate {
                    ref email,
                    ref reason,
                },
        } => {
            let person = EmailAddress::parse(email).context("--email is not an email address")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let mut tx = db.begin().await?;
            let decided = users::reactivate(&mut tx, &person, reason).await?;
            tx.commit().await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&decided)?
            )?;
            Ok(())
        }
        AdminCommand::Secrets {
            command: SecretsCommand::Rotate { batch },
        } => {
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(60)),
            )
            .await?;
            let keys = super::keys(&args.common)?;
            let report = secrets::rotate(&db, &keys, batch).await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&report)?
            )?;
            Ok(())
        }
        AdminCommand::Connections {
            command:
                ConnectionsCommand::Resync {
                    ref connection,
                    ref reason,
                },
        } => {
            let connection: Id<Connection> = connection
                .parse()
                .context("the connection is not a connection id (`con_…`)")?;
            let db = super::connect(
                &args.common,
                super::background_pool("norbelys-admin", 1, Duration::from_secs(30)),
            )
            .await?;
            let mut tx = db.begin().await?;
            let resynced = connections::resync(&mut tx, connection, reason).await?;
            tx.commit().await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&resynced)?
            )?;
            Ok(())
        }
        AdminCommand::Seed {
            ref slug,
            ref owner_email,
            ref mail,
            ref storage,
            ref identity,
            ref rendering,
        } => {
            let owner = EmailAddress::parse(owner_email)
                .context("--owner-email is not an email address")?;
            let seeded = seed::run(
                &seed::Environment {
                    common: &args.common,
                    mail,
                    storage,
                    identity,
                    rendering,
                },
                slug,
                &owner,
            )
            .await?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&seeded)?
            )?;
            Ok(())
        }
    }
}

/// Makes `owner` an owner of the `system` workspace (creating the user, or making an existing
/// membership an active owner) and mints an API key with every scope for it, its secret shown
/// once; written to the audit log.
async fn system_api_key(tx: &mut Tx, owner: &EmailAddress) -> anyhow::Result<workspaces::Created> {
    let system = crate::jobs::SYSTEM_WORKSPACE;
    let user = sqlx::query_scalar!(
        r#"INSERT INTO users (email, email_verified_at) VALUES ($1, now())
           ON CONFLICT (email_key) DO UPDATE SET updated_at = now()
           RETURNING id AS "id: Id<User>""#,
        owner.as_str()
    )
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO memberships (workspace_id, user_id, role, source) VALUES ($1, $2, 'owner', 'creator')
         ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = 'owner', status = 'active', status_changed_at = now()",
        system.uuid(),
        user.uuid()
    )
    .execute(&mut **tx)
    .await?;
    let (key, secret) = api_keys::create(
        tx,
        system,
        user,
        "System workspace key, created by the operator",
        ScopeSet::all(),
        KeyMode::Live,
        None,
    )
    .await?;
    audit::record(
        tx,
        system,
        AuditActor::Admin,
        Action::ApiKeyCreated,
        Some(key.to_string()),
        serde_json::json!({}),
        None,
    )
    .await?;
    Ok(workspaces::Created {
        workspace: system.id(),
        owner: user,
        api_key: key,
        api_key_secret: secret.secret,
    })
}
