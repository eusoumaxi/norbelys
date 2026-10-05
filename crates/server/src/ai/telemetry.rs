//! Telemetry of AI calls: one `ai.call` canonical event per call that reached the ledger (a
//! settlement, a release, an interruption, or a refusal for lack of budget), and the `ai_*`
//! metrics, the reviews AI asks of people among them. Labels come from closed sets (use case,
//! provider, outcome, direction, review reason); no prompt, answer, reason or violation text is
//! ever recorded, since they may hold personal data from a customer's mailbox or audience.

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use super::store::Settled;
use crate::domain::ai::{ReviewReason, Usage, UseCase};
use crate::domain::ids::{AiCall, Id, WorkspaceId};
use crate::jobs::JobId;

static CALLS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_ai_calls_total")
        .with_description("AI calls by use case, provider and outcome.")
        .build()
});

static TOKENS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_ai_tokens_total")
        .with_description("Tokens AI providers reported, by use case, provider and direction.")
        .build()
});

static COST: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_ai_cost_micros_total")
        .with_description("What settled AI calls were charged, in micro-dollars.")
        .build()
});

static REVIEWS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .u64_counter("norbelys_ai_reviews_requested_total")
        .with_description(
            "Reviews of AI verdicts asked of people, by use case and reason (low_confidence, sample).",
        )
        .build()
});

static LATENCY: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    opentelemetry::global::meter("norbelys")
        .f64_histogram("norbelys_ai_latency_seconds")
        .with_unit("s")
        .with_description("How long an AI call took, retries included.")
        .with_boundaries(vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0])
        .build()
});

/// One call as its event reports it.
#[derive(Debug, Clone)]
pub struct Report<'a> {
    /// The call's workspace.
    pub workspace: WorkspaceId,
    /// The job that made it.
    pub job: JobId,
    /// The call.
    pub call: Id<AiCall>,
    /// The use case.
    pub use_case: &'a str,
    /// The provider.
    pub provider: &'a str,
    /// The model.
    pub model: &'a str,
    /// The prompt's id and version.
    pub prompt_id: &'a str,
    /// The tokens the provider reported, if it did.
    pub usage: Option<Usage>,
    /// What the month was charged.
    pub charged: u64,
    /// `completed`, `refused`, `truncated`, `invalid_output`, `provider_error`, `timeout`,
    /// `released` or `quota_exceeded`.
    pub outcome: &'static str,
    /// How long the call took.
    pub elapsed: Duration,
}

/// Records one call: its event and its metrics.
pub fn call(report: &Report<'_>) {
    crate::telemetry::unit(crate::telemetry::Event::AiCall);
    tracing::info!(
        event = "ai.call",
        workspace_id = %report.workspace,
        job_id = %report.job,
        ai_call_id = %report.call,
        use_case = report.use_case,
        provider = report.provider,
        model = report.model,
        prompt_id = report.prompt_id,
        input_tokens = report.usage.map(|usage| usage.input_tokens),
        output_tokens = report.usage.map(|usage| usage.output_tokens),
        cost_micros = report.charged,
        outcome = report.outcome,
        duration_ms = u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        "AI call"
    );
    let labels = || {
        [
            KeyValue::new("use_case", report.use_case.to_owned()),
            KeyValue::new("provider", report.provider.to_owned()),
        ]
    };
    count(
        report.use_case,
        report.provider,
        report.outcome,
        report.charged,
    );
    if let Some(usage) = report.usage {
        for (direction, tokens) in [
            ("input", usage.input_tokens),
            ("output", usage.output_tokens),
        ] {
            let [use_case, provider] = labels();
            TOKENS.add(
                u64::from(tokens),
                &[use_case, provider, KeyValue::new("direction", direction)],
            );
        }
    }
    if report.outcome != "quota_exceeded" {
        LATENCY.record(report.elapsed.as_secs_f64(), &labels());
    }
}

/// Records a call the recovery of its abandoned run settled as interrupted.
pub fn interrupted(settled: &Settled) {
    crate::telemetry::unit(crate::telemetry::Event::AiCall);
    tracing::warn!(
        event = "ai.call",
        ai_call_id = %settled.id,
        use_case = %settled.use_case,
        provider = %settled.provider,
        model = %settled.model,
        prompt_id = %settled.prompt_id,
        cost_micros = settled.charged,
        outcome = "interrupted",
        "an abandoned AI call was settled at its reservation"
    );
    count(
        &settled.use_case,
        &settled.provider,
        "interrupted",
        settled.charged,
    );
}

/// Counts one call with its outcome and adds its charge.
fn count(use_case: &str, provider: &str, outcome: &'static str, charged: u64) {
    CALLS.add(
        1,
        &[
            KeyValue::new("use_case", use_case.to_owned()),
            KeyValue::new("provider", provider.to_owned()),
            KeyValue::new("outcome", outcome),
        ],
    );
    if charged > 0 {
        COST.add(
            charged,
            &[
                KeyValue::new("use_case", use_case.to_owned()),
                KeyValue::new("provider", provider.to_owned()),
            ],
        );
    }
}

/// Counts one review of an AI verdict asked of a person, by why: its low confidence, or the
/// random sample on which production precision and recall are measured. A day's increase is the
/// review queue's arrivals from AI, which the threshold and the sample share decide.
pub fn review_requested(use_case: UseCase, reason: ReviewReason) {
    REVIEWS.add(
        1,
        &[
            KeyValue::new("use_case", use_case.as_str()),
            KeyValue::new("reason", reason.as_str()),
        ],
    );
}
