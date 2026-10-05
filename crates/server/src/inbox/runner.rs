//! Inbox scheduling: polls receive bindings and records inbound mail through IMAP, the Gmail API
//! or Microsoft Graph.
//!
//! # The loop
//!
//! One claim loop per process. While a poll permit is free (`INBOX_PERMITS`, 32 by default), it
//! takes the turn of the workspace whose inbox turn is oldest among those with a due binding
//! ([`poll::turn`]), claims up to its free permits of that workspace's due bindings
//! ([`poll::due`], [`poll::claim`]), and runs each claimed poll in its own task, which holds its
//! permit until the poll is recorded. With nothing due it sleeps for [`IDLE`]: polls are due on a
//! clock (every five minutes by default, at once after a full page), so nothing needs to wake it
//! sooner. Every [`RECOVERY_EVERY`] it clears expired leases ([`poll::recover`]), so a binding
//! whose poller died is read again from its last committed cursor.
//!
//! # Shutdown
//!
//! On `SIGTERM` the loop stops claiming and polls under way get [`GRACE`] to finish; one that
//! outlives it leaves its lease to expire, and its fenced writes can no longer land.

use crate::db::Database;
use crate::inbox::poll::{self, Reader};
use crate::process::Shutdown;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;

/// The idle wait between turns when no binding is due.
const IDLE: Duration = Duration::from_secs(2);
/// How often expired leases are cleared.
const RECOVERY_EVERY: Duration = Duration::from_secs(15);
/// How long polls under way may finish after a shutdown request.
const GRACE: Duration = Duration::from_secs(35);

/// Claims and runs polls until shutdown, then waits for those under way within [`GRACE`].
pub(crate) async fn run(
    db: Database,
    reader: Arc<Reader>,
    slots: Arc<Semaphore>,
    owner: String,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    let mut tasks = JoinSet::new();
    let mut recovered = Instant::now().checked_sub(RECOVERY_EVERY);
    while !shutdown.requested() {
        while tasks.try_join_next().is_some() {}
        if recovered.is_none_or(|at| at.elapsed() >= RECOVERY_EVERY) {
            match poll::recover(&db).await {
                Ok(0) => {}
                Ok(cleared) => tracing::info!(cleared, "expired inbox leases cleared"),
                Err(error) => tracing::warn!(error = %error, "expired inbox leases not cleared"),
            }
            recovered = Some(Instant::now());
        }
        let claimed = claim_some(&db, &reader, &slots, &owner, &mut tasks).await;
        if claimed == 0 {
            tokio::select! {
                () = tokio::time::sleep(IDLE) => {}
                () = shutdown.wait() => {}
                Some(_) = tasks.join_next(), if slots.available_permits() == 0 => {}
            }
        }
    }
    if tokio::time::timeout(GRACE, async { while tasks.join_next().await.is_some() {} })
        .await
        .is_err()
    {
        tracing::warn!("polls still under way at the end of the grace; their leases will expire");
        tasks.abort_all();
    }
    Ok(())
}

/// One turn: claims what the free permits allow in the next workspace with due bindings and
/// spawns their polls; how many were claimed.
async fn claim_some(
    db: &Database,
    reader: &Arc<Reader>,
    slots: &Arc<Semaphore>,
    owner: &str,
    tasks: &mut JoinSet<()>,
) -> usize {
    let free = slots.available_permits();
    if free == 0 {
        return 0;
    }
    let workspace = match poll::turn(db).await {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return 0,
        Err(error) => {
            tracing::warn!(error = %error, "the inbox turn could not be taken");
            return 0;
        }
    };
    let due = match poll::due(db, workspace, free).await {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(error = %error, "due bindings could not be read");
            return 0;
        }
    };
    let mut claimed = 0;
    for binding in due {
        let Ok(permit) = Arc::clone(slots).try_acquire_owned() else {
            break;
        };
        let lease = match poll::claim(db, workspace, binding, owner).await {
            Ok(Some(lease)) => lease,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(error = %error, "a binding could not be claimed");
                continue;
            }
        };
        claimed += 1;
        let db = db.clone();
        let reader = Arc::clone(reader);
        tasks.spawn(async move {
            // A recorded poll, whatever its outcome, emits its own `inbox.poll` event.
            if let Err(error) = reader.poll(&db, &lease).await {
                tracing::warn!(binding = %lease.binding, error = %error, "a poll could not be recorded");
            }
            drop(permit);
        });
    }
    claimed
}
