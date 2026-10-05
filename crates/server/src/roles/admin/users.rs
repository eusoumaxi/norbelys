//! `norbelys-server admin users suspend` and `reactivate`: an operator's decision about a person's
//! account, as opposed to a workspace's decision about its members.
//!
//! A suspended person cannot sign in (every sign-in reads their standing), and a session of theirs
//! no longer proves anything (resolving it reads the user's standing too), so the suspension stops
//! them at once. In the same transaction it enqueues `sessions.revoke_user`, which revokes every
//! session they hold in batches, so a later reactivation revives none of them. Reactivating lets
//! them sign in again. Both decisions are recorded with the operator's reason in the person's own
//! log. Their memberships are left as they are: each workspace's administrators decide those.

use anyhow::Context as _;
use serde::Serialize;
use serde_json::json;

use crate::db::Tx;
use crate::domain::email::EmailAddress;
use crate::domain::ids::{Id, User};
use crate::identity::audit::{self, Action, AuditActor};
use crate::identity::sessions::RevokeUser;
use crate::jobs::{self, SYSTEM_WORKSPACE};

/// What a decision changed, as the command prints it.
#[derive(Debug, Serialize)]
pub(crate) struct Decided {
    /// The person.
    pub user: Id<User>,
    /// Their status now: `suspended` or `active`.
    pub status: &'static str,
}

/// The reason, refused when blank: it is what the person's log explains the decision with.
fn reason(reason: &str) -> anyhow::Result<&str> {
    let reason = reason.trim();
    anyhow::ensure!(
        !reason.is_empty(),
        "--reason is required: it is recorded in the person's log"
    );
    Ok(reason)
}

/// Suspends the person holding `email` for `reason` and enqueues the revocation of their sessions,
/// inside `tx` (see the module).
///
/// # Errors
///
/// No reason, nobody holds the address, or the database failed.
pub(crate) async fn suspend(
    tx: &mut Tx,
    email: &EmailAddress,
    reason: &str,
) -> anyhow::Result<Decided> {
    let reason = self::reason(reason)?;
    let user = sqlx::query_scalar!(
        r#"UPDATE users SET status = 'suspended' WHERE email_key = $1 RETURNING id AS "id: Id<User>""#,
        email.key(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .context("nobody holds this address")?;
    jobs::enqueue(
        tx,
        SYSTEM_WORKSPACE,
        &RevokeUser { user: user.uuid() },
        None,
    )
    .await?;
    audit::record_user(
        tx,
        user,
        AuditActor::System,
        Action::UserSuspended,
        Some(user.to_string()),
        json!({ "reason": reason }),
        None,
    )
    .await?;
    Ok(Decided {
        user,
        status: "suspended",
    })
}

/// Lets the suspended person holding `email` sign in again, for `reason`, inside `tx`. Their
/// revoked sessions stay revoked.
///
/// # Errors
///
/// No reason, no suspended person holds the address, or the database failed.
pub(crate) async fn reactivate(
    tx: &mut Tx,
    email: &EmailAddress,
    reason: &str,
) -> anyhow::Result<Decided> {
    let reason = self::reason(reason)?;
    let user = sqlx::query_scalar!(
        r#"UPDATE users SET status = 'active' WHERE email_key = $1 AND status = 'suspended'
           RETURNING id AS "id: Id<User>""#,
        email.key(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .context("no suspended person holds this address")?;
    audit::record_user(
        tx,
        user,
        AuditActor::System,
        Action::UserReactivated,
        Some(user.to_string()),
        json!({ "reason": reason }),
        None,
    )
    .await?;
    Ok(Decided {
        user,
        status: "active",
    })
}

#[cfg(test)]
mod tests {
    use super::{reactivate, suspend};
    use crate::domain::email::EmailAddress;
    use crate::domain::ids::{Id, User};
    use crate::identity::sessions::RevokeUser;
    use crate::jobs::runner::Harness;
    use crate::jobs::{Queue, Registry};
    use crate::testing::TestDb;

    /// The live and the suspension-revoked sessions of `user`.
    async fn sessions(test: &TestDb, user: Id<User>) -> (i64, i64) {
        sqlx::query_as(
            "SELECT count(*) FILTER (WHERE revoked_at IS NULL),
                    count(*) FILTER (WHERE revoked_reason = 'user_suspended')
               FROM sessions WHERE user_id = $1",
        )
        .bind(user.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap()
    }

    /// A suspension needs a reason, stops the person, and through `sessions.revoke_user` revokes
    /// every session they hold and no one else's; a reactivation lets them in again but revives no
    /// session; the person's own log records the sign-ins and both decisions in order. Without the
    /// revocation a reactivated account would find its old browsers signed in again.
    #[tokio::test]
    async fn a_suspension_revokes_every_session_and_a_reactivation_revives_none() {
        let test = TestDb::new().await;
        let ada = test.session("ada@example.com").await;
        test.session("ada@example.com").await;
        let grace = test.session("grace@example.com").await;
        let email = EmailAddress::parse("ada@example.com").unwrap();
        let mut tx = test.system.begin().await.unwrap();
        assert!(suspend(&mut tx, &email, " ").await.is_err());
        let suspended = suspend(&mut tx, &email, "Ticket 9: abuse").await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(suspended.user, ada.user);

        let mut registry = Registry::default();
        registry.register::<RevokeUser>().unwrap();
        let runner = Harness::new(
            test.worker.clone(),
            test.system.clone(),
            registry,
            http::Extensions::new(),
            "users-test",
        );
        while !runner.run_once(Queue::Maintenance, 1).await.is_empty() {}
        assert_eq!(sessions(&test, ada.user).await, (0, 2));
        assert_eq!(sessions(&test, grace.user).await, (1, 0));

        let mut tx = test.system.begin().await.unwrap();
        let active = reactivate(&mut tx, &email, "Ticket 9: resolved")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(active.status, "active");
        assert_eq!(sessions(&test, ada.user).await, (0, 2));
        let mut tx = test.system.begin().await.unwrap();
        assert!(reactivate(&mut tx, &email, "twice").await.is_err());

        let log: Vec<String> =
            sqlx::query_scalar("SELECT action FROM user_audit_log WHERE user_id = $1 ORDER BY id")
                .bind(ada.user.uuid())
                .fetch_all(test.system.pool())
                .await
                .unwrap();
        assert_eq!(
            log,
            [
                "session.created",
                "session.created",
                "user.suspended",
                "user.reactivated"
            ]
        );
    }
}
