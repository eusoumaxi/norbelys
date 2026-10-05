//! AI in the server: the deployment's providers and models, the durable call every use case
//! makes, and the use cases the job kinds call.
//!
//! # Where AI decides
//!
//! Rules decide first; AI decides only where they are silent, records that it decided, never
//! produces an irreversible effect alone and never runs on the delivery path. Every use case
//! runs inside a job of the `ai` queue: the classification of inbound messages the rules left
//! open ([`classify::classify`]) and the personalisation snippets of a campaign step
//! ([`snippets::generate`]). Each answers under a JSON schema its provider enforces, and its
//! answer is checked again by the use case's own types.
//!
//! # The durable call
//!
//! [`call`] makes one call in four steps:
//!
//! 1. **Heartbeat.** The job's lease is renewed, so a run that lost its lease starts nothing.
//! 2. **Reserve** ([`store::reserve`]). The call's most possible cost (its input bound and its
//!    output limit at the model's prices) is reserved against the workspace's monthly budget,
//!    with one `ai_calls` row holding the month and the price snapshot. A month that cannot
//!    admit it refuses the call (`quota_exceeded`) and its people hear once that the budget is
//!    exhausted.
//! 3. **Call** the use case's [`norbelys_ai::Client`], within its deadline (at most 90 seconds,
//!    the provider's retries included). The lease is renewed every 20 seconds meanwhile; a
//!    renewal that fails abandons the call, so no request outlives the lease that started it.
//! 4. **Settle** ([`store::settle`]), once. An answer of any kind is charged the usage the
//!    provider reported; an error the provider does not bill is charged nothing; an answer lost
//!    after sending, or a deadline, is charged the reservation; a call never sent is released.
//!
//! # Recovery
//!
//! A run can end between steps 2 and 4: its process dies, its lease is lost, the database
//! fails while settling. The AI job kinds declare [`store::settle_abandoned`] as their recovery
//! hook, which the job runner calls inside the transaction that ends every claim of the job
//! (concluded, or recovered after its lease expired): each reservation still open is settled as
//! interrupted at its bound before the job can run again. A retry is a new call with a new row.
//!
//! # Pauses
//!
//! A provider's rate limit (`429`) that outlasts the call's retries pauses the use case in this
//! process for the provider's `Retry-After`. Calls of the use case are not made meanwhile; the
//! job waits in its lane and tries again later.
//!
//! # Prompts and canaries
//!
//! A call uses its use case's current prompt ([`prompts`]). While a newer version of
//! classification's prompt is declared as a canary, [`Ai::prompt`] serves it to a random 5 % of
//! the messages for a day, guarded by its review rate against the current prompt's over the same
//! period, read at most once a minute from the call rows of every workspace
//! ([`store::canary_stats`]): a rise beyond the deployment's margin (`AI_CANARY_MARGIN_POINTS`)
//! rolls it back, a day within it promotes it. Each call's row records its prompt and whether it
//! served as a canary, so the outcome survives restarts and is the same in every process.
//!
//! # Privacy
//!
//! Prompts, answers, reasons and violations are never logged, traced or put in an error: the
//! canonical event and the metrics carry counts, costs and outcomes only ([`telemetry`]), and
//! the client's span follows the OpenTelemetry generative AI conventions without content.
//!
//! Lock order: a job's call rows, then the month's usage row (`ai/store.rs`).

pub mod classify;
#[cfg(test)]
mod eval;
#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod hints;
pub mod prompts;
pub mod review;
pub mod snippets;
pub mod store;
pub mod telemetry;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use norbelys_ai::{AiError, Client, Completion, Message, Request, RetrySchedule};
use serde_json::{Map, Value};
use tracing::Instrument as _;

use self::prompts::Prompt;
use self::store::{NewCall, Reserved};
use self::telemetry::Report;
use crate::config::AiArgs;
use crate::db::Database;
use crate::domain::ai::{
    AiSettings, CallOutcome, Ending, ModelEntry, Phase, Provider, RouteError, Serve, Usage,
    UseCase, canary_phase, input_token_bound, reservation_micros, route, serve, settle,
};
use crate::domain::ids::{AiCall, Id};
use crate::domain::retry;
use crate::jobs::{JobContext, JobError};
use crate::problem::{FieldError, Problem};

/// How often the lease of a job waiting on a provider is renewed: three renewals fit in one
/// lease, so a single slow renewal never lets it expire.
const HEARTBEAT: Duration = Duration::from_secs(20);

/// How long a canary's phase is reused before its guard's figures are read again.
const CANARY_REFRESH: Duration = Duration::from_secs(60);

/// The deployment's AI: how each use case is served, when it can be, and the pauses providers
/// asked for. Built once by the worker and shared by every job through the runner's
/// environment; cloning it is cheap.
#[derive(Debug, Clone)]
pub struct Ai {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    routes: Vec<Route>,
    paused: Mutex<Vec<(UseCase, Instant)>>,
    /// How far, in points of review rate, a canary prompt's may exceed the current prompt's.
    canary_margin: u32,
    /// The phase of each use case's canary and when it was read, so the guard's figures are read
    /// once a minute per process, not once per call.
    canaries: Mutex<Vec<(UseCase, Instant, Phase)>>,
}

/// The prompt of one use-case invocation, chosen once so that a corrective call reuses it.
#[derive(Debug, Clone, Copy)]
pub struct Served {
    /// The prompt.
    pub prompt: &'static Prompt,
    /// Whether it serves as a canary, which the call's row records for the canary's guard.
    pub canary: bool,
}

/// How one use case is served: its model's catalogue entry and the client that calls it.
#[derive(Debug, Clone)]
pub struct Route {
    /// The use case.
    pub use_case: UseCase,
    /// The model, its provider, its prices and its schema support.
    pub entry: ModelEntry,
    /// The configured client of the model.
    pub client: Client,
}

/// Why the deployment's AI configuration was refused at start.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A use case's model is not priced, or the capability matrix does not mark it strict.
    #[error(transparent)]
    Route(#[from] RouteError),
    /// A client could not be built (a plain `http` URL to a public host, a key that is not a
    /// header value).
    #[error("the AI client of {use_case} cannot be built: {error}")]
    Client {
        /// The use case.
        use_case: &'static str,
        /// What the client refused.
        #[source]
        error: AiError,
    },
}

impl Ai {
    /// The deployment's AI from its configuration: each use case's model priced by the catalogue
    /// and checked against the capability matrix (an unpriced model, or one the matrix does not
    /// mark strict, is refused here, at start, before any job runs), and a client for it when its
    /// provider has a key. A use case whose provider has no key is unavailable, which is logged
    /// once.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a use case that could never be served as configured.
    pub fn from_args(args: &AiArgs) -> Result<Self, ConfigError> {
        Self::build(
            args,
            &[
                (UseCase::Classification, &args.ai_classification_model),
                (UseCase::Snippets, &args.ai_snippets_model),
            ],
        )
    }

    #[cfg(test)]
    fn for_evaluation(args: &AiArgs) -> Result<Self, ConfigError> {
        Self::build(
            args,
            &[
                (UseCase::Classification, &args.ai_classification_model),
                (UseCase::Snippets, &args.ai_snippets_model),
                (UseCase::Hints, &args.ai_hints_model),
            ],
        )
    }

    fn build(
        args: &AiArgs,
        cases: &[(UseCase, &crate::domain::ai::ModelRef)],
    ) -> Result<Self, ConfigError> {
        let timeout = Duration::from_secs(args.ai_timeout_seconds);
        let mut routes = Vec::new();
        for &(use_case, model) in cases {
            let entry = route(use_case, model, &args.ai_models)?;
            let (base_url, key) = match entry.model.provider {
                Provider::Anthropic => (&args.anthropic_base_url, &args.anthropic_api_key),
                Provider::OpenAi => (&args.openai_base_url, &args.openai_api_key),
            };
            let Some(key) = key else {
                tracing::info!(
                    use_case = use_case.as_str(),
                    provider = entry.model.provider.as_str(),
                    "AI use case unavailable: its provider has no key"
                );
                continue;
            };
            let client = Client::new(&norbelys_ai::Provider {
                wire: entry.model.provider.wire(),
                base_url: base_url.clone(),
                model: entry.model.model.clone(),
                api_key: key.clone(),
                // `route` admitted the model only because the capability matrix marks it strict.
                structured_output: true,
                timeout,
                retries: schedule(),
            })
            .map_err(|error| ConfigError::Client {
                use_case: use_case.as_str(),
                error,
            })?;
            routes.push(Route {
                use_case,
                entry: entry.clone(),
                client,
            });
        }
        let today = jiff::Zoned::now().date();
        if today
            .since(args.ai_prices_read_on)
            .is_ok_and(|age| age.get_days() > 90)
        {
            tracing::warn!(
                read_on = %args.ai_prices_read_on,
                "the AI catalogue's prices were read more than 90 days ago; check the providers' pricing pages"
            );
        }
        Ok(Self {
            inner: Arc::new(Inner {
                routes,
                paused: Mutex::new(Vec::new()),
                canary_margin: args.ai_canary_margin_points,
                canaries: Mutex::new(Vec::new()),
            }),
        })
    }

    /// The prompt `use_case` uses for one invocation: its current prompt, unless a canary is
    /// declared for it ([`prompts::canary`]); then the canary's phase decides
    /// ([`canary_phase`]): while it is measured it serves a random share of the invocations as a
    /// canary, rolled back it serves none, promoted it serves all. The phase is read from the
    /// call rows of every workspace at most once a minute per process; if they cannot be read,
    /// the current prompt serves.
    pub async fn prompt(&self, db: &Database, use_case: UseCase) -> Served {
        let current = prompts::current(use_case);
        let Some(canary) = prompts::canary(use_case) else {
            return Served {
                prompt: current,
                canary: false,
            };
        };
        let Some(phase) = self.phase(db, use_case, canary, current).await else {
            return Served {
                prompt: current,
                canary: false,
            };
        };
        match serve(phase, crate::jobs::draw()) {
            Serve::Current => Served {
                prompt: current,
                canary: false,
            },
            Serve::Canary => Served {
                prompt: canary,
                canary: true,
            },
            Serve::Promoted => Served {
                prompt: canary,
                canary: false,
            },
        }
    }

    /// The phase of `use_case`'s canary: cached for a minute, else read from the call rows.
    /// `None` when they cannot be read, which is logged.
    async fn phase(
        &self,
        db: &Database,
        use_case: UseCase,
        canary: &Prompt,
        current: &Prompt,
    ) -> Option<Phase> {
        let fresh = |read: Instant| read.elapsed() < CANARY_REFRESH;
        let cached = self
            .inner
            .canaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(named, read, _)| *named == use_case && fresh(*read))
            .map(|(.., phase)| *phase);
        if cached.is_some() {
            return cached;
        }
        let stats = match store::canary_stats(db, use_case, &canary.id, &current.id).await {
            Ok(stats) => stats,
            Err(error) => {
                tracing::warn!(
                    use_case = use_case.as_str(),
                    error = %error,
                    "the canary's guard could not be read; the current prompt serves"
                );
                return None;
            }
        };
        let phase = canary_phase(&stats, jiff::Timestamp::now(), self.inner.canary_margin);
        let mut canaries = self
            .inner
            .canaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        canaries.retain(|(named, ..)| *named != use_case);
        canaries.push((use_case, Instant::now(), phase));
        Some(phase)
    }

    /// How `use_case` is served, or `None` when its provider has no key.
    #[must_use]
    pub fn route(&self, use_case: UseCase) -> Option<&Route> {
        self.inner
            .routes
            .iter()
            .find(|route| route.use_case == use_case)
    }

    /// How long `use_case` stays paused after a provider's rate limit, if it is.
    #[must_use]
    pub fn paused(&self, use_case: UseCase) -> Option<Duration> {
        let now = Instant::now();
        let paused = self
            .inner
            .paused
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        paused
            .iter()
            .find(|(paused, _)| *paused == use_case)
            .and_then(|(_, until)| until.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    /// Pauses `use_case` for `wait`, or longer if it is paused longer already.
    fn pause(&self, use_case: UseCase, wait: Duration) {
        let until = Instant::now() + wait;
        let mut paused = self
            .inner
            .paused
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match paused.iter_mut().find(|(paused, _)| *paused == use_case) {
            Some((_, current)) => *current = (*current).max(until),
            None => paused.push((use_case, until)),
        }
    }
}

/// The retries of every AI call: the `AI` backoff of the one retry module, with its fixed number
/// of retries; the client lets a provider's `Retry-After` take precedence.
fn schedule() -> RetrySchedule {
    RetrySchedule {
        max_retries: retry::AI_RETRIES,
        wait: Arc::new(|retries| retry::backoff(retries, &retry::AI, crate::jobs::draw())),
    }
}

/// One call a use case makes.
#[derive(Debug)]
pub struct Call<'a> {
    /// The use case.
    pub use_case: UseCase,
    /// Its prompt, the system prompt of the call, whose id the call's row records.
    pub prompt: &'a Prompt,
    /// Whether the prompt serves as a canary ([`Served::canary`]), which the call's row records.
    pub canary: bool,
    /// The conversation: the input, and after an invalid answer that answer and its violation.
    pub messages: Vec<Message>,
    /// The JSON schema of the answer.
    pub schema: Value,
    /// The most output tokens, thinking included: it bounds the reservation.
    pub max_tokens: u32,
    /// When the answer stops being useful: the call is abandoned then, as a deadline.
    pub deadline: Option<tokio::time::Instant>,
}

/// What a call's answer came to. The provider billed every variant.
pub enum Answer<T> {
    /// The answer passed the schema and the use case's checks: the value, and the call's row,
    /// on which a use case records what the answer led to ([`store::record_review`]).
    Valid(T, Id<AiCall>),
    /// The model declined to answer.
    Refused,
    /// The answer was cut at its output limit.
    Truncated,
    /// The answer is not valid: what the model wrote, and what is wrong with it, written to be
    /// shown to the model in one corrective call. Neither is ever logged.
    Invalid {
        /// The model's answer.
        text: String,
        /// What is wrong with it.
        violation: String,
    },
}

/// Why a call produced no [`Answer`].
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The deployment has no provider key for the use case.
    #[error("no AI provider serves this use case")]
    Unavailable,
    /// The use case is paused after a provider's rate limit, for this long.
    #[error("the AI use case is paused after a rate limit")]
    Paused(Duration),
    /// The month's budget does not admit the call; nothing was spent.
    #[error("the month's AI budget does not admit the call")]
    OverBudget,
    /// The provider gave no answer; the call is settled as the error says.
    #[error("the AI provider gave no answer: {0}")]
    Provider(AiError),
    /// The job's lease was lost or the database failed; an unsettled reservation is left to the
    /// recovery hook.
    #[error(transparent)]
    Job(#[from] JobError),
}

/// Makes one durable call for the job `cx` runs (see the module): renews the lease, reserves,
/// calls the use case's model while renewing the lease, checks a finished answer with `check`,
/// settles once, and reports it.
///
/// # Errors
///
/// [`CallError`]: the use case is unavailable or paused, the budget refused it, the provider
/// gave no answer, or the job's lease or database failed.
pub async fn call<T: Send>(
    cx: &mut JobContext,
    ai: &Ai,
    call: Call<'_>,
    check: impl FnOnce(&Value) -> Result<T, String> + Send,
) -> Result<Answer<T>, CallError> {
    let use_case = call.use_case;
    let route = ai.route(use_case).ok_or(CallError::Unavailable)?;
    if let Some(left) = ai.paused(use_case) {
        return Err(CallError::Paused(left));
    }
    let id = Id::<AiCall>::new();
    let request = Request {
        call_id: id.to_string(),
        system: call.prompt.text.to_owned(),
        messages: call.messages,
        max_tokens: call.max_tokens,
        // Current Claude models and OpenAI's reasoning models refuse an explicit temperature.
        temperature: None,
        schema: Some(call.schema),
    };
    let reserved = reservation_micros(
        input_token_bound(prompt_bytes(&request)),
        request.max_tokens,
        route.entry.price,
    );
    let span = tracing::info_span!(
        "ai.call",
        norbelys.ai.use_case = use_case.as_str(),
        norbelys.ai.prompt_id = %call.prompt.id,
        norbelys.ai_call_id = %id,
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    let prompt = call.prompt;
    let canary = call.canary;
    let deadline = call.deadline;
    let result = async move {
        cx.heartbeat().await?;
        let started = Instant::now();
        let report = |cx: &JobContext, usage: Option<Usage>, charged: u64, outcome| Report {
            workspace: cx.workspace(),
            job: cx.id(),
            call: id,
            use_case: use_case.as_str(),
            provider: route.entry.model.provider.as_str(),
            model: &route.entry.model.model,
            prompt_id: &prompt.id,
            usage,
            charged,
            outcome,
            elapsed: started.elapsed(),
        };
        let new = NewCall {
            id,
            job: cx.id(),
            use_case,
            entry: &route.entry,
            prompt_id: &prompt.id,
            canary,
            reserved,
        };
        let reserved_now = store::reserve(cx.db(), cx.workspace(), &new, crate::process::now())
            .await
            .map_err(JobError::from)?;
        if reserved_now == Reserved::Refused {
            telemetry::call(&report(cx, None, 0, "quota_exceeded"));
            return Err(CallError::OverBudget);
        }
        let result = complete(cx, &route.client, &request, deadline).await?;
        let (ending, usage, answer) = interpret(result, check, id);
        if let Err(CallError::Provider(AiError::RateLimited {
            retry_after: Some(wait),
        })) = &answer
        {
            ai.pause(use_case, *wait);
        }
        let settlement = settle(ending, usage, route.entry.price, reserved);
        let settled = store::settle(cx.db(), cx.workspace(), id, &settlement)
            .await
            .map_err(JobError::from)?;
        if settled.is_some() {
            let outcome = settlement.outcome.map_or("released", CallOutcome::as_str);
            telemetry::call(&report(cx, settlement.usage, settlement.charged, outcome));
        }
        answer
    }
    .instrument(span.clone())
    .await;
    if let Err(error) = &result {
        span.record("otel.status_code", "ERROR");
        let code = match error {
            CallError::Unavailable => "unavailable",
            CallError::Paused(_) => "throttled",
            CallError::OverBudget => "quota_exceeded",
            CallError::Provider(_) => "provider",
            CallError::Job(error) => error.code(),
        };
        span.record("error.type", code);
    }
    result
}

/// The bytes of a request's system prompt, turns and schema: the base of its input bound.
fn prompt_bytes(request: &Request) -> usize {
    request.system.len()
        + request
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>()
        + request
            .schema
            .as_ref()
            .map_or(0, |schema| schema.to_string().len())
}

/// Waits for the call while renewing the job's lease every [`HEARTBEAT`]; abandons the call when
/// a renewal fails (the error is returned) or when `deadline` passes (a timeout).
async fn complete(
    cx: &mut JobContext,
    client: &Client,
    request: &Request,
    deadline: Option<tokio::time::Instant>,
) -> Result<Result<Completion, AiError>, JobError> {
    let call = client.complete(request);
    tokio::pin!(call);
    let until = async move {
        match deadline {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(until);
    loop {
        tokio::select! {
            result = &mut call => return Ok(result),
            () = &mut until => return Ok(Err(AiError::Timeout)),
            () = tokio::time::sleep(HEARTBEAT) => cx.heartbeat().await?,
        }
    }
}

/// How the call `id` ended, as settlement sees it, with the provider's usage and what the caller
/// gets: a finished JSON answer is checked by `check`, and one that fails is invalid.
fn interpret<T>(
    result: Result<Completion, AiError>,
    check: impl FnOnce(&Value) -> Result<T, String>,
    id: Id<AiCall>,
) -> (Ending, Option<Usage>, Result<Answer<T>, CallError>) {
    use norbelys_ai::{Outcome, Output};
    match result {
        Ok(completion) => {
            let usage = completion.usage.map(|usage| Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            });
            let (ending, answer) = match completion.outcome {
                Outcome::Completed(Output::Json(value)) => match check(&value) {
                    Ok(valid) => (Ending::Completed, Answer::Valid(valid, id)),
                    Err(violation) => (
                        Ending::InvalidOutput,
                        Answer::Invalid {
                            text: value.to_string(),
                            violation,
                        },
                    ),
                },
                Outcome::Completed(Output::Text(text)) => (
                    Ending::InvalidOutput,
                    Answer::Invalid {
                        text,
                        violation: "the answer is not a JSON object".to_owned(),
                    },
                ),
                Outcome::Refused { .. } => (Ending::Refused, Answer::Refused),
                Outcome::Truncated { .. } => (Ending::Truncated, Answer::Truncated),
                Outcome::InvalidOutput { text, violation } => {
                    (Ending::InvalidOutput, Answer::Invalid { text, violation })
                }
            };
            (ending, usage, Ok(answer))
        }
        Err(error) => {
            let ending = match &error {
                AiError::Config(_) => Ending::NotSent,
                AiError::Unreachable
                | AiError::RateLimited { .. }
                | AiError::Unauthorized { .. }
                | AiError::Provider { .. }
                | AiError::Rejected { .. } => Ending::NotBilled,
                AiError::Transport | AiError::InvalidResponse => Ending::Lost,
                AiError::Timeout => Ending::TimedOut,
            };
            (ending, None, Err(CallError::Provider(error)))
        }
    }
}

/// Checks the `ai` member of a workspace's settings as `PATCH /workspaces/{id}` receives them:
/// each top-level member given replaces the stored one, so `ai` is checked whole. Absent or
/// `null` is fine (the defaults apply); anything else must read as the AI settings
/// ([`AiSettings::parse`]), and every invalid field is answered at once, its pointer under
/// `/settings/ai`.
///
/// # Errors
///
/// `422 validation_failed` with one entry per invalid field.
pub fn check_settings(settings: &Map<String, Value>) -> Result<(), Problem> {
    let Some(ai) = settings.get("ai") else {
        return Ok(());
    };
    AiSettings::parse(Some(ai)).map(|_| ()).map_err(|errors| {
        Problem::validation(
            errors
                .into_iter()
                .map(|error| FieldError {
                    pointer: format!("/settings/ai{}", error.pointer),
                    code: "invalid".to_owned(),
                    detail: error.detail.to_owned(),
                })
                .collect(),
        )
    })
}
