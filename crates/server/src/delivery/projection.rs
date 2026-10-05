//! The slot projection: how the next twelve five-minute slots (one hour) are loaded with known
//! sends, exported every minute as `norbelys_slot_planned_sends{slot, class}`.
//!
//! The numbers come from the database view `slot_projection`, which estimates, for each slot,
//! the cold sends of paced senders (`paced`: each mailbox's due rows placed one interval apart
//! from its clock) and the mail placed by its due time alone (`due`: rate-paced connections and
//! mail created through the API). It assumes every window open, every budget and breaker clear
//! and nothing new arriving, so it shows how the coming slots are loaded, not what they will
//! deliver; set beside the sends actually made, it shows whether the grid keeps up.
//!
//! The worker reads it as the scheduler role, so the gauge covers every workspace. The app login
//! may read the same view, where row security limits it to one workspace; no API operation
//! serves it yet.

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Gauge;

use crate::db::{self, Database};
use crate::process::Shutdown;

/// How often the projection is read.
const EVERY: Duration = Duration::from_secs(60);

static PLANNED: LazyLock<Gauge<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_gauge("norbelys_slot_planned_sends")
        .with_description(
            "Known sends planned in each of the next twelve five-minute slots (slot 0 is the \
             current one), by class: paced cold mail, or mail placed by its due time.",
        )
        .build()
});

/// One slot of the projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// 0 for the current slot, up to 11.
    pub offset: i32,
    /// Cold sends of paced senders planned in it.
    pub paced: i64,
    /// Sends placed in it by their due time.
    pub due: i64,
}

/// Reads the projection of every workspace, as the scheduler.
///
/// # Errors
///
/// The database refused.
pub async fn read(db: &Database) -> Result<Vec<Slot>, sqlx::Error> {
    let mut tx = db.begin().await?;
    db::as_scheduler(&mut tx).await?;
    let slots = sqlx::query_as!(
        Slot,
        r#"SELECT slot_offset AS "offset!", paced AS "paced!", due AS "due!"
             FROM slot_projection ORDER BY slot_offset"#,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(slots)
}

/// Exports the projection every minute until `shutdown`. A failed read is logged and tried again
/// at the next minute.
pub async fn export(db: Database, mut shutdown: Shutdown) {
    loop {
        match read(&db).await {
            Ok(slots) => {
                for slot in slots {
                    let offset = KeyValue::new("slot", i64::from(slot.offset));
                    for (class, count) in [("paced", slot.paced), ("due", slot.due)] {
                        PLANNED.record(
                            u64::try_from(count).unwrap_or(0),
                            &[offset.clone(), KeyValue::new("class", class)],
                        );
                    }
                }
            }
            Err(error) => tracing::warn!(error = %error, "the slot projection could not be read"),
        }
        tokio::select! {
            () = tokio::time::sleep(EVERY) => {}
            () = shutdown.wait() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{SenderSpec, TestDb};

    /// The projection is read across workspaces through the scheduler role, with twelve slots,
    /// and a message due now through the API counts in the current slot's `due` class: the metric
    /// sees the whole grid, not one tenant's share of it.
    #[tokio::test]
    async fn the_projection_covers_twelve_slots_of_every_workspace() {
        let test = TestDb::new().await;
        for slug in ["acme", "globex"] {
            let ws = test.workspace(slug).await.id;
            let sender = test
                .sender(ws, &SenderSpec::relay(&format!("hello@{slug}.test")))
                .await;
            test.direct_message(ws, &sender, &["ada@example.com"], -60)
                .await;
        }
        let slots = read(&test.worker).await.unwrap();
        assert_eq!(slots.len(), 12);
        assert_eq!(
            slots.first().map(|slot| (slot.offset, slot.due)),
            Some((0, 2))
        );
        assert_eq!(slots.iter().map(|slot| slot.due).sum::<i64>(), 2);
    }
}
