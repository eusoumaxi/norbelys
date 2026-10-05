//! The ledger of AI calls: one `ai_calls` row per call and one `ai_usage` row per workspace and
//! UTC month, written in the workspace's transactions.
//!
//! - **Reserve** ([`reserve`]): before a call, its bound is added to the month's open
//!   reservations and its row is inserted with the month, the price snapshot and the bound, in
//!   one transaction; or nothing is reserved when the month's budget does not admit it. The
//!   same transaction first settles any reservation an earlier run of the same job left open,
//!   so a retry never runs beside an unsettled call of its own.
//! - **Settle** ([`settle`]): after the call, once. The row's update is fenced on
//!   `state = 'reserved'`, so of two settlers (the call's own and a recovery) exactly one
//!   changes the row, and only that one moves the month's money: its reservation leaves
//!   `reserved_cost_micros` and its charge joins `cost_micros`, in the month of the
//!   reservation, whatever the clock says.
//! - **Abandoned calls** ([`settle_abandoned`]): the recovery hook of the AI job kinds. Inside
//!   the transaction that ends a claim of the job, every reservation of the job still open is
//!   settled as interrupted at its full bound, since what the provider billed is unknown.
//! - **Verdicts** ([`record_review`]): a classification's settled call records whether its
//!   verdict fell below the threshold. With the prompt and the canary flag the row holds since
//!   its reservation, that is what a canary prompt's guard reads ([`canary_stats`]), across
//!   workspaces as the scheduler, which sees only these columns of verdict rows.
//!
//! Notices: after every change of a month's spend, and at every refusal, the month's due
//! notices (`ai.budget_warning`, `ai.budget_exceeded`) are recorded in the outbox once, marked by
//! `ai_usage.warned_at` and `exceeded_at` in the same transaction. A budget change clears both
//! marks, so the notices follow the budget in force.
//!
//! Lock order: a job's call rows, then the month's usage row. Every path that takes both
//! (reserving, which first sweeps the job's open rows, settling, the hook) takes them in this
//! order; reserving inserts its own row after locking the usage row, which waits on nothing,
//! because the row is new.

use serde_json::json;

use crate::db::{Database, Tx};
use crate::domain::ai::{
    AiSettings, CallState, CanaryStats, ModelEntry, Settlement, UseCase, Verdicts, admits,
    month_of, notices,
};
use crate::domain::ids::{AiCall, Id, WorkspaceId};
use crate::domain::time::{Date, Timestamp};
use crate::jobs::JobId;
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// A call about to be reserved.
#[derive(Debug, Clone)]
pub struct NewCall<'a> {
    /// The call's id, chosen by the caller so the request can carry it.
    pub id: Id<AiCall>,
    /// The job making the call: the recovery of its claims settles what it leaves open.
    pub job: JobId,
    /// The use case.
    pub use_case: UseCase,
    /// The model, its provider and its prices, copied into the row.
    pub entry: &'a ModelEntry,
    /// The prompt's id and version, `classification/reply-v1`.
    pub prompt_id: &'a str,
    /// Whether the prompt serves as a canary, so the canary's guard counts the call.
    pub canary: bool,
    /// The bound of the call's bill, in micro-dollars.
    pub reserved: u64,
}

/// Whether a reservation was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserved {
    /// The reservation is held: the call may be made.
    Admitted,
    /// The month's budget does not admit the call; nothing was reserved.
    Refused,
}

/// A call row settled, as telemetry reports it.
#[derive(Debug, Clone)]
pub struct Settled {
    /// The use case.
    pub use_case: String,
    /// The provider.
    pub provider: String,
    /// The model.
    pub model: String,
    /// The prompt's id and version.
    pub prompt_id: String,
    /// The call's id.
    pub id: Id<AiCall>,
    /// What the month was charged.
    pub charged: u64,
}

/// A workspace's AI spend for one UTC month, as the workspace's `usage` shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct MonthUsage {
    /// The month, as its first day.
    pub month: Date,
    /// Calls settled this month, interrupted ones included.
    pub calls: i64,
    /// What settled calls cost, in micro-dollars.
    pub spent_micros: i64,
    /// The bounds of the calls still running, in micro-dollars.
    pub reserved_micros: i64,
    /// The month's budget, in micro-dollars.
    pub budget_micros: i64,
}

/// A count of micro-dollars as PostgreSQL stores it (`bigint`), saturating.
fn stored(micros: u64) -> i64 {
    i64::try_from(micros).unwrap_or(i64::MAX)
}

/// A stored count of micro-dollars back as unsigned (never negative: the table checks it).
fn amount(micros: i64) -> u64 {
    u64::try_from(micros).unwrap_or(0)
}

/// Reads `workspace`'s AI settings inside `tx`. Settings that do not read (stored before a rule
/// changed, or written around the API) turn every use case off and the budget to zero, with a
/// warning: AI never runs on settings nobody can see.
///
/// # Errors
///
/// The database failed.
pub async fn read_settings(tx: &mut Tx, workspace: WorkspaceId) -> Result<AiSettings, sqlx::Error> {
    let stored = sqlx::query_scalar!(
        r#"SELECT settings -> 'ai' AS "ai" FROM workspaces WHERE id = $1"#,
        workspace.uuid()
    )
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    Ok(AiSettings::parse(stored.as_ref()).unwrap_or_else(|errors| {
        tracing::warn!(
            workspace_id = %workspace,
            invalid = errors.len(),
            "the workspace's AI settings do not read; AI is off for it"
        );
        AiSettings {
            classify_replies: false,
            generate_snippets: false,
            monthly_budget_micros: 0,
            ..AiSettings::default()
        }
    }))
}

/// Reserves `call` in `workspace` at `now`: settles first what earlier runs of the same job left
/// open, then locks the month's usage row (creating it with the budget in force, and re-arming
/// the notices when the budget changed), then either inserts the call's row and adds its bound
/// to the month's reservations, or, when the budget does not admit it, records the exhaustion
/// notice if it is due. Commits either way.
///
/// # Errors
///
/// The database failed; nothing was reserved.
pub async fn reserve(
    db: &Database,
    workspace: WorkspaceId,
    call: &NewCall<'_>,
    now: Timestamp,
) -> Result<Reserved, sqlx::Error> {
    let month = Date(month_of(now.0));
    let mut tx = db.begin_in(workspace).await?;
    sweep(&mut tx, workspace, call.job).await?;
    let budget = read_settings(&mut tx, workspace)
        .await?
        .monthly_budget_micros;
    let usage = sqlx::query!(
        r#"INSERT INTO ai_usage (workspace_id, month, budget_micros) VALUES ($1, $2, $3)
           ON CONFLICT (workspace_id, month) DO UPDATE SET budget_micros = EXCLUDED.budget_micros,
                  warned_at = CASE WHEN ai_usage.budget_micros = EXCLUDED.budget_micros THEN ai_usage.warned_at END,
                  exceeded_at = CASE WHEN ai_usage.budget_micros = EXCLUDED.budget_micros THEN ai_usage.exceeded_at END
           RETURNING cost_micros, reserved_cost_micros, warned_at IS NOT NULL AS "warned!",
                     exceeded_at IS NOT NULL AS "exceeded!""#,
        workspace.uuid(),
        month as _,
        stored(budget),
    )
    .fetch_one(&mut *tx)
    .await?;
    let spent = amount(usage.cost_micros);
    if !admits(
        budget,
        spent,
        amount(usage.reserved_cost_micros),
        call.reserved,
    ) {
        notify(
            &mut tx,
            workspace,
            month,
            Month {
                spent,
                budget,
                warned: usage.warned,
                exceeded: usage.exceeded,
            },
            true,
        )
        .await?;
        tx.commit().await?;
        return Ok(Reserved::Refused);
    }
    sqlx::query!(
        "INSERT INTO ai_calls (workspace_id, id, job_id, use_case, provider, model, prompt_id, canary, month, reserved_micros,
                               price_input_micros_per_mtok, price_output_micros_per_mtok)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        workspace.uuid(),
        call.id.uuid(),
        call.job.uuid(),
        call.use_case.as_str(),
        call.entry.model.provider.as_str(),
        call.entry.model.model,
        call.prompt_id,
        call.canary,
        month as _,
        stored(call.reserved),
        stored(call.entry.price.input),
        stored(call.entry.price.output),
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE ai_usage SET reserved_cost_micros = reserved_cost_micros + $3
          WHERE workspace_id = $1 AND month = $2",
        workspace.uuid(),
        month as _,
        stored(call.reserved),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Reserved::Admitted)
}

/// Settles `call` of `workspace` as `settlement` says, once: `None` when the row was settled
/// already (by the recovery of an abandoned run, say), in which case nothing changes. Otherwise
/// the month of the reservation is charged, its reservation released, and its due notices
/// recorded, in one transaction.
///
/// # Errors
///
/// The database failed; the call stays reserved for the recovery hook to settle.
pub async fn settle(
    db: &Database,
    workspace: WorkspaceId,
    call: Id<AiCall>,
    settlement: &Settlement,
) -> Result<Option<Settled>, sqlx::Error> {
    let mut tx = db.begin_in(workspace).await?;
    let row = sqlx::query!(
        r#"UPDATE ai_calls SET state = $3, outcome = $4, input_tokens = $5, output_tokens = $6,
                  settled_micros = $7, finished_at = now()
            WHERE workspace_id = $1 AND id = $2 AND state = 'reserved'
           RETURNING month AS "month: Date", reserved_micros, use_case, provider, model, prompt_id"#,
        workspace.uuid(),
        call.uuid(),
        settlement.state.as_str(),
        settlement.outcome.map(|outcome| outcome.as_str()),
        settlement
            .usage
            .map(|usage| i32::try_from(usage.input_tokens).unwrap_or(i32::MAX)),
        settlement
            .usage
            .map(|usage| i32::try_from(usage.output_tokens).unwrap_or(i32::MAX)),
        stored(settlement.charged),
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let counted = i32::from(settlement.state != CallState::Released);
    charge(
        &mut tx,
        workspace,
        row.month,
        settlement.charged,
        amount(row.reserved_micros),
        counted,
    )
    .await?;
    tx.commit().await?;
    Ok(Some(Settled {
        use_case: row.use_case,
        provider: row.provider,
        model: row.model,
        prompt_id: row.prompt_id,
        id: call,
        charged: settlement.charged,
    }))
}

/// The recovery hook of the AI job kinds, run by the job runner inside the transaction that ends
/// a claim of the job (its conclusion, or the recovery of its expired lease), in the job's
/// workspace: every reservation of the job still open is settled as interrupted at its full
/// bound, before the job can run again. What the provider billed for an abandoned call is
/// unknown, and the bound is the most it can have cost. A kind declares it as
/// `const RECOVERY_HOOK: Option<RecoveryHook> = Some(ai::store::settle_abandoned);`.
///
/// # Errors
///
/// The database failed; the runner's transaction rolls back and the next sweep tries again.
pub fn settle_abandoned<'a>(
    tx: &'a mut Tx,
    workspace: WorkspaceId,
    job: JobId,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), sqlx::Error>> + Send + 'a>> {
    Box::pin(async move {
        for settled in sweep(tx, workspace, job).await? {
            super::telemetry::interrupted(&settled);
        }
        Ok(())
    })
}

/// Settles every open reservation of `job` as interrupted at its bound, inside `tx`; returns the
/// rows it settled.
async fn sweep(
    tx: &mut Tx,
    workspace: WorkspaceId,
    job: JobId,
) -> Result<Vec<Settled>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"UPDATE ai_calls SET state = 'interrupted', outcome = 'interrupted', settled_micros = reserved_micros,
                  finished_at = now()
            WHERE workspace_id = $1 AND job_id = $2 AND state = 'reserved'
           RETURNING id AS "id: Id<AiCall>", month AS "month: Date", reserved_micros, use_case, provider, model, prompt_id"#,
        workspace.uuid(),
        job.uuid(),
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut settled = Vec::with_capacity(rows.len());
    for row in rows {
        let reserved = amount(row.reserved_micros);
        charge(tx, workspace, row.month, reserved, reserved, 1).await?;
        settled.push(Settled {
            use_case: row.use_case,
            provider: row.provider,
            model: row.model,
            prompt_id: row.prompt_id,
            id: row.id,
            charged: reserved,
        });
    }
    Ok(settled)
}

/// Moves one settled call's money in `month`: `charged` joins the spend, `reserved` leaves the
/// open reservations, `counted` (0 or 1) joins the calls; then records the notices now due.
async fn charge(
    tx: &mut Tx,
    workspace: WorkspaceId,
    month: Date,
    charged: u64,
    reserved: u64,
    counted: i32,
) -> Result<(), sqlx::Error> {
    let usage = sqlx::query!(
        r#"UPDATE ai_usage SET cost_micros = cost_micros + $3, reserved_cost_micros = reserved_cost_micros - $4,
                  calls = calls + $5
            WHERE workspace_id = $1 AND month = $2
           RETURNING cost_micros, budget_micros, warned_at IS NOT NULL AS "warned!",
                     exceeded_at IS NOT NULL AS "exceeded!""#,
        workspace.uuid(),
        month as _,
        stored(charged),
        stored(reserved),
        counted,
    )
    .fetch_one(&mut **tx)
    .await?;
    notify(
        tx,
        workspace,
        month,
        Month {
            spent: amount(usage.cost_micros),
            budget: amount(usage.budget_micros),
            warned: usage.warned,
            exceeded: usage.exceeded,
        },
        false,
    )
    .await
}

/// A month's spend and which notices it already gave.
#[derive(Debug, Clone, Copy)]
struct Month {
    spent: u64,
    budget: u64,
    warned: bool,
    exceeded: bool,
}

/// Records the notices `month` calls for and has not given yet, in the outbox, and marks them
/// given on its usage row, whose lock the caller holds. Recording the exhaustion also marks the
/// warning given: a warning after the budget is exhausted would only confuse.
async fn notify(
    tx: &mut Tx,
    workspace: WorkspaceId,
    month: Date,
    state: Month,
    refused: bool,
) -> Result<(), sqlx::Error> {
    let due = notices(state.spent, state.budget, refused);
    let warn = due.warning && !state.warned;
    let exhaust = due.exceeded && !state.exceeded;
    if !warn && !exhaust {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE ai_usage SET warned_at = coalesce(warned_at, now()),
                exceeded_at = CASE WHEN $3 THEN coalesce(exceeded_at, now()) ELSE exceeded_at END
          WHERE workspace_id = $1 AND month = $2",
        workspace.uuid(),
        month as _,
        exhaust,
    )
    .execute(&mut **tx)
    .await?;
    let data = json!({
        "month": month,
        "spent_micros": state.spent,
        "budget_micros": state.budget,
    });
    for (kind, due) in [
        (EventType::AiBudgetWarning, warn),
        (EventType::AiBudgetExceeded, exhaust),
    ] {
        if due {
            outbox::record(
                tx,
                workspace,
                Event {
                    kind,
                    subject_type: "workspace",
                    subject_id: workspace.uuid(),
                    data: data.clone(),
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// `workspace`'s AI spend in the UTC month of `now`: zero spend under the budget in force when
/// nothing was reserved this month yet.
///
/// # Errors
///
/// The database failed.
pub async fn month_usage(
    tx: &mut Tx,
    workspace: WorkspaceId,
    now: Timestamp,
) -> Result<MonthUsage, sqlx::Error> {
    let month = Date(month_of(now.0));
    let row = sqlx::query!(
        "SELECT calls, cost_micros, reserved_cost_micros, budget_micros FROM ai_usage
          WHERE workspace_id = $1 AND month = $2",
        workspace.uuid(),
        month as _,
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match row {
        Some(row) => MonthUsage {
            month,
            calls: i64::from(row.calls),
            spent_micros: row.cost_micros,
            reserved_micros: row.reserved_cost_micros,
            budget_micros: row.budget_micros,
        },
        None => MonthUsage {
            month,
            calls: 0,
            spent_micros: 0,
            reserved_micros: 0,
            budget_micros: stored(read_settings(tx, workspace).await?.monthly_budget_micros),
        },
    })
}

/// Records on `call` of `workspace` what its verdict led to: `requested` when the verdict fell
/// below the workspace's confidence threshold. A sampled review is recorded as `false`: the sample
/// is drawn at random and says nothing about the prompt. This is the review rate a canary prompt
/// is guarded by. A call not settled as completed is left as it is.
///
/// # Errors
///
/// The database failed.
pub async fn record_review(
    db: &Database,
    workspace: WorkspaceId,
    call: Id<AiCall>,
    requested: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_in(workspace).await?;
    sqlx::query!(
        "UPDATE ai_calls SET review_requested = $3
          WHERE workspace_id = $1 AND id = $2 AND state = 'settled' AND outcome = 'completed'",
        workspace.uuid(),
        call.uuid(),
        requested,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// What the guard of `canary`, a canary prompt of `use_case`, reads from the call rows of every
/// workspace, as the scheduler: the canary's verdicts as a canary; the `current` prompt's verdicts
/// from the canary's first verdict to its last, the same traffic over the same period; and
/// whether the canary has given a verdict as the use case's prompt, which only its promotion
/// does. Only verdict rows are read, through their partial index.
///
/// # Errors
///
/// The database failed.
pub async fn canary_stats(
    db: &Database,
    use_case: UseCase,
    canary: &str,
    current: &str,
) -> Result<CanaryStats, sqlx::Error> {
    let mut tx = db.begin().await?;
    crate::db::as_scheduler(&mut tx).await?;
    let row = sqlx::query!(
        r#"WITH canary AS (
               SELECT min(started_at) AS first, max(started_at) AS last, count(*) AS verdicts,
                      count(*) FILTER (WHERE review_requested) AS reviews
                 FROM ai_calls
                WHERE use_case = $1 AND prompt_id = $2 AND canary AND review_requested IS NOT NULL
           )
           SELECT c.first AS "first?: Timestamp", c.verdicts AS "canary_verdicts!",
                  c.reviews AS "canary_reviews!",
                  (SELECT count(*) FROM ai_calls a
                    WHERE a.use_case = $1 AND a.prompt_id = $3 AND a.review_requested IS NOT NULL
                      AND a.started_at BETWEEN c.first AND c.last) AS "current_verdicts!",
                  (SELECT count(*) FROM ai_calls a
                    WHERE a.use_case = $1 AND a.prompt_id = $3 AND a.review_requested
                      AND a.started_at BETWEEN c.first AND c.last) AS "current_reviews!",
                  EXISTS (SELECT 1 FROM ai_calls a
                           WHERE a.use_case = $1 AND a.prompt_id = $2 AND NOT a.canary
                             AND a.review_requested IS NOT NULL) AS "promoted!"
             FROM canary c"#,
        use_case.as_str(),
        canary,
        current,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let count = |count: i64| u64::try_from(count).unwrap_or(0);
    Ok(CanaryStats {
        first: row.first.map(|first| first.0),
        canary: Verdicts {
            verdicts: count(row.canary_verdicts),
            reviews: count(row.canary_reviews),
        },
        current: Verdicts {
            verdicts: count(row.current_verdicts),
            reviews: count(row.current_reviews),
        },
        promoted: row.promoted,
    })
}
