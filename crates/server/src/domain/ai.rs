//! AI: the pure rules of the platform's use of language models. Which use cases exist, which
//! configured model may serve one, what a call may cost before it starts and what it cost
//! after, when a workspace's monthly budget admits a call and when its people are told, how
//! every way a call can end is settled, when a classification goes to a person, how a new
//! prompt version is tried on a share of calls, and a workspace's AI settings.
//!
//! # Models
//!
//! The deployment's catalogue prices each model it may use; whether the provider enforces a JSON
//! schema for a model is the AI client's tested capability matrix, never configuration. A use
//! case is served only by a priced model the matrix marks strict ([`route`]), which is decided
//! when the worker starts.
//!
//! # Reviews
//!
//! A classification below the workspace's confidence threshold goes to a person. So does a
//! uniform random sample of the confident ones ([`review`], `settings.ai.review_sample`): a
//! model's self-reported confidence is not calibration, and the people's decisions on that sample
//! measure each class's precision and recall in production, which is what the threshold should
//! be set from.
//!
//! # Canaries
//!
//! A new prompt version of classification first serves [`CANARY_SHARE`] of the calls
//! ([`serve`]) for [`CANARY_PERIOD`], guarded by its review rate: the share of its verdicts below
//! the threshold, against the current prompt's over the same period. A rise beyond the
//! deployment's margin rolls it back; a period within the margin promotes it ([`canary_phase`]).
//! Both outcomes are read from the call rows alone, which record each call's prompt, whether it
//! served as a canary and whether its verdict asked for a review.
//!
//! # Money
//!
//! Amounts are integers of micro-dollars (millionths of a US dollar), never floating point:
//! a budget, a reservation and a settlement add and subtract exactly. Prices are micro-dollars
//! per million tokens, as providers publish them (USD per million tokens), so a model at
//! $1 per million input tokens costs one micro-dollar per input token. A cost is rounded up to
//! the next micro-dollar, so the sum of settlements never undercounts a provider's bill.
//!
//! # Reservation and settlement
//!
//! Before a call, its most possible cost is reserved against the month's budget: the bound of
//! its input tokens times the input price, plus its output limit times the output price
//! ([`reservation_micros`]). Input and output are priced separately and the output limit
//! bounds the answer, so this is a bound on the whole bill. The input bound needs no tokenizer:
//! every token of today's providers' tokenizers spans at least one byte of UTF-8, so a prompt
//! of `n` bytes is at most `n` tokens, plus a fixed allowance for the formatting and the
//! instructions a provider adds around a request ([`input_token_bound`]). The bound is used to
//! reserve, never to settle: a settlement charges the usage the provider reported, or the
//! reservation when the usage is unknown ([`settle`]).
//!
//! A reservation is admitted only while the month's settled spend, its open reservations and
//! the new one together stay within the budget ([`admits`]).
//!
//! # Notices
//!
//! A workspace's people hear about its budget twice a month at most: a warning once the
//! settled spend reaches 80 % of the budget, and a notice that the budget is exhausted the
//! first time a call is refused for lack of budget or the settled spend reaches 100 %
//! ([`notices`]). Which notices were already given is the caller's state; a budget change
//! re-arms both.
//!
//! # Months
//!
//! Budgets are per UTC month ([`month_of`]). A call belongs to the month it was reserved in,
//! whatever the clock says when it settles, so a call reserved at 23:59 on the last day of a
//! month is charged to that month even if its answer arrives after midnight.

pub mod redact;

use std::fmt;
use std::str::FromStr;

use jiff::SignedDuration;
use jiff::civil::Date;
use norbelys_ai::Wire;
use serde_json::Value;

use super::people;

/// A use of AI by the platform. Each has its own prompt, schema, model and switch.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum UseCase {
    /// Classifying an inbound message the rules left as a human reply or unknown: human reply,
    /// auto reply, out of office or unknown, with a sentiment.
    Classification,
    /// Personalisation snippets of a campaign step, written from the person's usable fields.
    Snippets,
    /// List hygiene hints: the risk of an address, from a person's fields. Its prompt and
    /// evaluation exist so the use case can be added as one job kind; nothing calls it yet.
    Hints,
}

impl UseCase {
    /// The use case as stored in `ai_calls.use_case` and used as a metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// An AI provider the deployment can be configured with. Each speaks one wire format and has
/// one deployment key.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
pub enum Provider {
    /// Anthropic's Messages API.
    #[strum(serialize = "anthropic")]
    Anthropic,
    /// OpenAI's chat completions, or any server that speaks that format (a self-hosted model).
    #[strum(serialize = "openai")]
    OpenAi,
}

impl Provider {
    /// The provider as stored in `ai_calls.provider`, named in configuration and used as a
    /// metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The wire format the provider speaks: Anthropic's Messages API, or chat completions.
    #[must_use]
    pub fn wire(self) -> Wire {
        match self {
            Self::Anthropic => Wire::Anthropic,
            Self::OpenAi => Wire::OpenAiCompatible,
        }
    }
}

/// What a model costs, in micro-dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    /// Per million input tokens.
    pub input: u64,
    /// Per million output tokens.
    pub output: u64,
}

/// A model as configuration names it: `provider/model`, such as `anthropic/claude-haiku-4-5`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    /// The provider that serves it.
    pub provider: Provider,
    /// The model id the provider knows it by.
    pub model: String,
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider.as_str(), self.model)
    }
}

/// Why a model, a catalogue entry or an amount could not be read from configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Not `provider/model` with a known provider and a model id of visible ASCII.
    #[error(
        "`{0}` is not `provider/model`: the provider is `anthropic` or `openai`, the model id 1 to 128 visible ASCII characters"
    )]
    Model(String),
    /// Not `provider/model=input:output`.
    #[error("`{0}` is not `provider/model=input:output`, prices in USD per million tokens")]
    Entry(String),
    /// Not a dollar amount with at most the allowed decimals.
    #[error("`{0}` is not an amount of US dollars with at most {1} decimals")]
    Amount(String, usize),
}

impl FromStr for ModelRef {
    type Err = ConfigError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || ConfigError::Model(text.to_owned());
        let (provider, model) = text.trim().split_once('/').ok_or_else(invalid)?;
        let provider = provider.parse::<Provider>().map_err(|_| invalid())?;
        if model.is_empty() || model.len() > 128 || !model.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid());
        }
        Ok(Self {
            provider,
            model: model.to_owned(),
        })
    }
}

/// One model of the deployment's catalogue: its price. Prices change often, so they are
/// configuration; whether the provider enforces a JSON schema for the model is not, since it is a
/// claim that must be tested: the capability matrix of the AI client
/// ([`norbelys_ai::matrix`]) states it, and [`route`] consults it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// The provider and the model.
    pub model: ModelRef,
    /// What a call costs.
    pub price: Price,
}

impl FromStr for ModelEntry {
    type Err = ConfigError;

    /// Reads `provider/model=input:output`, prices in US dollars per million tokens with at most
    /// six decimals: `anthropic/claude-haiku-4-5=1:5`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || ConfigError::Entry(text.to_owned());
        let (model, prices) = text.trim().split_once('=').ok_or_else(invalid)?;
        let model = model.parse::<ModelRef>()?;
        let (input, output) = prices.split_once(':').ok_or_else(invalid)?;
        Ok(Self {
            model,
            price: Price {
                input: parse_usd(input, 6).map_err(|_| invalid())?,
                output: parse_usd(output, 6).map_err(|_| invalid())?,
            },
        })
    }
}

/// The most US dollars an amount may name: a budget or a price per million tokens above it is
/// a typo, not a setting.
const USD_MAX: u64 = 1_000_000;

/// Reads a non-negative amount of US dollars written in decimal, with at most `decimals`
/// decimals (at most 6), into micro-dollars exactly: `0.10` is 100,000. Exponents, signs and
/// amounts above one million dollars are refused.
///
/// # Errors
///
/// [`ConfigError::Amount`] when `text` is not such an amount.
pub fn parse_usd(text: &str, decimals: usize) -> Result<u64, ConfigError> {
    let invalid = || ConfigError::Amount(text.to_owned(), decimals);
    let (whole, fraction) = text.trim().split_once('.').unwrap_or((text.trim(), ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty()
        || whole.len() > 7
        || !digits(whole)
        || !digits(fraction)
        || fraction.len() > decimals.min(6)
        || (text.contains('.') && fraction.is_empty())
    {
        return Err(invalid());
    }
    let whole: u64 = whole.parse().map_err(|_| invalid())?;
    let mut micros: u64 = 0;
    let mut scale: u64 = 100_000;
    for digit in fraction.bytes() {
        micros += u64::from(digit - b'0') * scale;
        scale /= 10;
    }
    if whole > USD_MAX || (whole == USD_MAX && micros > 0) {
        return Err(invalid());
    }
    Ok(whole * 1_000_000 + micros)
}

/// Why a use case cannot be served by the model configured for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// The model is not in the catalogue, so its price is unknown and no call can be reserved.
    #[error("{use_case} is configured with {model}, which the AI catalogue does not price")]
    Unpriced {
        /// The use case.
        use_case: &'static str,
        /// The configured model.
        model: String,
    },
    /// The capability matrix says the provider does not enforce a JSON schema for this model;
    /// every use case answers under a schema, so it can never be served.
    #[error(
        "{use_case} is configured with {model}, which is known not to enforce a JSON schema (strict structured output); choose a model the capability matrix marks strict"
    )]
    NotStrict {
        /// The use case.
        use_case: &'static str,
        /// The configured model.
        model: String,
    },
    /// The capability matrix has no row for this provider and model: whether it enforces a JSON
    /// schema was never tested, so it is not supported.
    #[error(
        "{use_case} is configured with {model}, which the capability matrix has not tested for strict structured output; choose a model it lists as strict, or evaluate this one and add its row"
    )]
    Untested {
        /// The use case.
        use_case: &'static str,
        /// The configured model.
        model: String,
    },
}

/// The catalogue entry that serves `use_case` with `model`: refused when the catalogue has no
/// price for it, and unless the capability matrix ([`norbelys_ai::matrix`]) says the provider
/// enforces a JSON schema for it, through the provider's wire. Decided once when the
/// deployment's configuration is read, so an unsupported combination stops the process at start,
/// before any job of the use case runs, instead of failing every one of them.
///
/// # Errors
///
/// [`RouteError`] for an unpriced model, one known not to enforce a schema, or an untested one.
pub fn route<'a>(
    use_case: UseCase,
    model: &ModelRef,
    catalogue: &'a [ModelEntry],
) -> Result<&'a ModelEntry, RouteError> {
    let entry = catalogue
        .iter()
        .find(|entry| entry.model == *model)
        .ok_or_else(|| RouteError::Unpriced {
            use_case: use_case.as_str(),
            model: model.to_string(),
        })?;
    match norbelys_ai::matrix::strict(model.provider.wire(), &model.model) {
        Some(true) => Ok(entry),
        Some(false) => Err(RouteError::NotStrict {
            use_case: use_case.as_str(),
            model: model.to_string(),
        }),
        None => Err(RouteError::Untested {
            use_case: use_case.as_str(),
            model: model.to_string(),
        }),
    }
}

/// Tokens allowed, beyond one per byte of the prompt, for the formatting of a request and the
/// instructions a provider adds to enforce a schema. Providers do not publish the size of the
/// latter; a thousand tokens is several times what they add today.
pub const REQUEST_OVERHEAD_TOKENS: u64 = 1_024;

/// An upper bound on the input tokens of a request whose system prompt, turns and schema hold
/// `prompt_bytes` bytes of UTF-8: one token per byte at most, plus [`REQUEST_OVERHEAD_TOKENS`].
#[must_use]
pub fn input_token_bound(prompt_bytes: usize) -> u64 {
    u64::try_from(prompt_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(REQUEST_OVERHEAD_TOKENS)
}

/// What `input_tokens` and `output_tokens` cost at `price`, in micro-dollars, rounded up.
#[must_use]
pub fn cost_micros(input_tokens: u64, output_tokens: u64, price: Price) -> u64 {
    let total = u128::from(input_tokens) * u128::from(price.input)
        + u128::from(output_tokens) * u128::from(price.output);
    u64::try_from(total.div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

/// The reservation of a call: its input bound and its output limit at `price`, the most the
/// call can be billed.
#[must_use]
pub fn reservation_micros(input_bound: u64, max_output_tokens: u32, price: Price) -> u64 {
    cost_micros(input_bound, u64::from(max_output_tokens), price)
}

/// Whether a month whose settled spend is `spent` and whose open reservations are `reserved`
/// admits a new reservation of `wanted` under `budget`: all three together stay within it.
#[must_use]
pub fn admits(budget: u64, spent: u64, reserved: u64, wanted: u64) -> bool {
    u128::from(spent) + u128::from(reserved) + u128::from(wanted) <= u128::from(budget)
}

/// The notices a month's spend calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Notices {
    /// `ai.budget_warning`: the settled spend reached 80 % of the budget.
    pub warning: bool,
    /// `ai.budget_exceeded`: a call was refused for lack of budget, or the settled spend
    /// reached the budget.
    pub exceeded: bool,
}

/// The notices due for a month whose settled spend is `spent` under `budget`, when a call was
/// just `refused` for lack of budget (or not). Nothing is due before anything was spent, except
/// the notice of a refusal. Whether a due notice was already given is the caller's state.
#[must_use]
pub fn notices(spent: u64, budget: u64, refused: bool) -> Notices {
    let spent_any = spent > 0;
    Notices {
        warning: spent_any && u128::from(spent) * 5 >= u128::from(budget) * 4,
        exceeded: refused || (spent_any && spent >= budget),
    }
}

/// The UTC month of `at`, as its first day: the key of a month's budget and usage.
#[must_use]
pub fn month_of(at: jiff::Timestamp) -> Date {
    let day = at.to_zoned(jiff::tz::TimeZone::UTC).date();
    day.first_of_month()
}

/// The tokens a provider reported for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u32,
    /// Output tokens, thinking included.
    pub output_tokens: u32,
}

/// How a call ended, as its settlement sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Ending {
    /// The model finished and the answer passed the use case's checks.
    Completed,
    /// The model declined to answer.
    Refused,
    /// The answer was cut at its output limit or the model's context window.
    Truncated,
    /// The answer does not follow the schema, or failed the use case's checks.
    InvalidOutput,
    /// The provider answered with an error it does not bill (a rate limit, a failure, a refused
    /// key or request), or could not be reached: nothing was billed.
    NotBilled,
    /// The answer was lost after the request was sent (a broken connection, an unreadable
    /// answer): the provider may have billed it, at an unknown amount.
    Lost,
    /// The deadline passed before a complete answer: the provider may have billed it, at an
    /// unknown amount.
    TimedOut,
    /// The call was refused before anything was sent (an invalid configuration or request):
    /// nothing was billed.
    NotSent,
    /// The run that made the call was abandoned (its process died or lost its lease) before the
    /// call was settled: what happened is unknown.
    Interrupted,
}

/// The state of a call's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum CallState {
    /// Reserved; the call has not ended.
    Reserved,
    /// Ended and charged.
    Settled,
    /// Never sent; the reservation is given back.
    Released,
    /// Abandoned; charged at its reservation.
    Interrupted,
}

impl CallState {
    /// The state as stored in `ai_calls.state`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The recorded outcome of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum CallOutcome {
    /// A usable answer.
    Completed,
    /// The model declined.
    Refused,
    /// The answer was cut short.
    Truncated,
    /// The answer was not valid.
    InvalidOutput,
    /// The provider failed, refused the request, or the answer was lost.
    ProviderError,
    /// The deadline passed.
    Timeout,
    /// The run was abandoned.
    Interrupted,
}

impl CallOutcome {
    /// The outcome as stored in `ai_calls.outcome` and used as a metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// How a call is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// The row's final state.
    pub state: CallState,
    /// The recorded outcome; none for a call that was never sent.
    pub outcome: Option<CallOutcome>,
    /// What the month is charged, in micro-dollars.
    pub charged: u64,
    /// The usage recorded on the row, when the provider reported it.
    pub usage: Option<Usage>,
}

/// The settlement of a call that ended as `ending`, with the usage the provider reported, at
/// the call's `price` and its `reserved` amount:
///
/// - an answer of any kind (completed, refused, truncated, invalid) was billed: it is charged
///   its reported usage, or its reservation when the provider reported none;
/// - an error the provider does not bill is charged nothing;
/// - an answer lost after sending, a deadline and an abandoned run may have been billed at an
///   unknown amount: they are charged the reservation, the most the call could cost;
/// - a call never sent is released and charged nothing.
#[must_use]
pub fn settle(ending: Ending, usage: Option<Usage>, price: Price, reserved: u64) -> Settlement {
    let billed = |outcome: CallOutcome| Settlement {
        state: CallState::Settled,
        outcome: Some(outcome),
        charged: usage.map_or(reserved, |usage| {
            cost_micros(
                u64::from(usage.input_tokens),
                u64::from(usage.output_tokens),
                price,
            )
        }),
        usage,
    };
    let unknown = |state: CallState, outcome: CallOutcome, charged: u64| Settlement {
        state,
        outcome: Some(outcome),
        charged,
        usage: None,
    };
    match ending {
        Ending::Completed => billed(CallOutcome::Completed),
        Ending::Refused => billed(CallOutcome::Refused),
        Ending::Truncated => billed(CallOutcome::Truncated),
        Ending::InvalidOutput => billed(CallOutcome::InvalidOutput),
        Ending::NotBilled => unknown(CallState::Settled, CallOutcome::ProviderError, 0),
        Ending::Lost => unknown(CallState::Settled, CallOutcome::ProviderError, reserved),
        Ending::TimedOut => unknown(CallState::Settled, CallOutcome::Timeout, reserved),
        Ending::Interrupted => unknown(CallState::Interrupted, CallOutcome::Interrupted, reserved),
        Ending::NotSent => Settlement {
            state: CallState::Released,
            outcome: None,
            charged: 0,
            usage: None,
        },
    }
}

/// Why a person is asked to review an AI classification instead of it being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum ReviewReason {
    /// The verdict's confidence is below the workspace's threshold.
    LowConfidence,
    /// The verdict was confident enough to apply, and was drawn into the sample a person checks.
    Sample,
}

impl ReviewReason {
    /// The reason as a metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Whether a verdict of `confidence` is applied (`None`) or reviewed by a person, and why: below
/// `threshold` it is reviewed for its low confidence; at or above it, a `sample` share of
/// verdicts, chosen by the uniformly random `draw`, is reviewed anyway. The sample is uniform
/// over every confident verdict, so the people's decisions on it estimate the precision and the
/// recall of each class in production, which a model's self-reported confidence cannot.
#[must_use]
pub fn review(confidence: f64, threshold: f64, sample: f64, draw: u64) -> Option<ReviewReason> {
    if confidence < threshold {
        Some(ReviewReason::LowConfidence)
    } else if sampled(sample, draw) {
        Some(ReviewReason::Sample)
    } else {
        None
    }
}

/// Whether the uniformly random 64-bit `draw` falls in a `share` (from 0 to 1) of all draws: its
/// upper 32 bits, read as a fraction of 2^32, are below the share. A share of 0 never samples and
/// a share of 1 always does.
#[must_use]
pub fn sampled(share: f64, draw: u64) -> bool {
    let unit = f64::from(u32::try_from(draw >> 32).unwrap_or(u32::MAX)) / 4_294_967_296.0;
    unit < share
}

/// The share of calls a canary prompt serves while it is measured.
pub const CANARY_SHARE: f64 = 0.05;
/// How long a canary serves its share, from its first verdict, before it can be promoted.
pub const CANARY_PERIOD: SignedDuration = SignedDuration::from_hours(24);
/// The fewest verdicts a canary needs before its guard may roll it back or promote it: fewer say
/// too little about its review rate. A canary that has not reached them serves on past its period.
pub const CANARY_VERDICTS_MIN: u64 = 50;

/// The verdicts one prompt version gave: how many, and how many asked a person to review for
/// low confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Verdicts {
    /// Calls that gave a verdict.
    pub verdicts: u64,
    /// Of those, the verdicts below the confidence threshold.
    pub reviews: u64,
}

impl Verdicts {
    /// The review rate: the share of verdicts that asked for a review, 0 without verdicts.
    #[must_use]
    pub fn rate(self) -> f64 {
        let count = |count: u64| f64::from(u32::try_from(count).unwrap_or(u32::MAX));
        if self.verdicts == 0 {
            0.0
        } else {
            count(self.reviews) / count(self.verdicts)
        }
    }
}

/// What a canary's guard reads from the call rows of every workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CanaryStats {
    /// When the canary gave its first verdict as a canary, if it has.
    pub first: Option<jiff::Timestamp>,
    /// The canary's verdicts as a canary.
    pub canary: Verdicts,
    /// The current prompt's verdicts from the canary's first verdict to its last: the same
    /// traffic over the same period.
    pub current: Verdicts,
    /// Whether the canary has served a call as the use case's prompt, which only a promotion does.
    pub promoted: bool,
}

/// Where a canary prompt stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Phase {
    /// The canary serves its share of calls ([`CANARY_SHARE`]); the current prompt serves the rest.
    Serving,
    /// Its review rate rose beyond the margin: the current prompt serves every call.
    RolledBack,
    /// It served its period within the margin: it serves every call.
    Promoted,
}

/// The phase of a canary at `now`, from `stats` and the margin, in points of review rate, by which
/// its review rate may exceed the current prompt's:
///
/// - **promoted** once it served a call as the use case's prompt: a promotion is final;
/// - **rolled back** once it has [`CANARY_VERDICTS_MIN`] verdicts and its review rate exceeds the
///   current prompt's by more than the margin. This is final without any stored state: a
///   rolled-back canary serves no more calls, so its verdicts stop changing, and the current
///   prompt's are counted only up to the canary's last verdict, so they stop changing too;
/// - **promoted** once [`CANARY_PERIOD`] has passed since its first verdict, with enough verdicts
///   within the margin;
/// - **serving** otherwise: before its first call, while it has too few verdicts to judge, and
///   until its period ends.
#[must_use]
pub fn canary_phase(stats: &CanaryStats, now: jiff::Timestamp, margin_points: u32) -> Phase {
    if stats.promoted {
        return Phase::Promoted;
    }
    let Some(first) = stats.first else {
        return Phase::Serving;
    };
    if stats.canary.verdicts < CANARY_VERDICTS_MIN {
        return Phase::Serving;
    }
    if stats.canary.rate() > stats.current.rate() + f64::from(margin_points) / 100.0 {
        Phase::RolledBack
    } else if now.duration_since(first) >= CANARY_PERIOD {
        Phase::Promoted
    } else {
        Phase::Serving
    }
}

/// Which prompt one call uses while its use case has a canary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serve {
    /// The current prompt.
    Current,
    /// The canary, as a canary: the call counts towards its guard.
    Canary,
    /// The canary, promoted: the use case's prompt now.
    Promoted,
}

/// The prompt of one call in `phase`, chosen by the uniformly random `draw`: while the canary is
/// serving, it takes [`CANARY_SHARE`] of the calls.
#[must_use]
pub fn serve(phase: Phase, draw: u64) -> Serve {
    match phase {
        Phase::Serving if sampled(CANARY_SHARE, draw) => Serve::Canary,
        Phase::Serving | Phase::RolledBack => Serve::Current,
        Phase::Promoted => Serve::Promoted,
    }
}

/// A person's own attributes a workspace may mark usable by AI. The address is not among them:
/// an address is never sent to a model.
pub const OWN_FIELDS: [&str; 3] = ["given_name", "family_name", "company"];

/// The most fields a workspace may mark usable: every custom field definition and the own
/// attributes.
const USABLE_FIELDS_MAX: usize = 103;

/// A workspace's AI settings, `workspaces.settings.ai`: which use cases are on, the monthly
/// budget, the confidence a classification needs, the share of confident classifications a
/// person checks, and which of a person's fields may be sent to a model. Every field has a
/// default, so an absent or partial object is complete.
#[derive(Debug, Clone, PartialEq)]
pub struct AiSettings {
    /// Whether inbound messages the rules leave as a human reply or unknown are classified by
    /// AI. Off by default: a workspace's owner turns it on, since it sends reply excerpts to a
    /// provider.
    pub classify_replies: bool,
    /// Whether campaign steps that ask for personalisation snippets get them written by AI. On
    /// by default: a step's author asks for snippets explicitly, and no field of a person is
    /// sent until the workspace marks it usable.
    pub generate_snippets: bool,
    /// The month's budget, in micro-dollars (`monthly_budget_usd` on the wire, at most two
    /// decimals). Ten dollars by default.
    pub monthly_budget_micros: u64,
    /// The confidence, from 0 to 1, a classification needs to be applied; below it a person is
    /// asked to review. 0.7 by default.
    pub confidence_threshold: f64,
    /// The share, from 0 to 1, of classifications confident enough to apply that a person
    /// reviews anyway, drawn at random ([`review`]). 0.05 by default. A model's own confidence
    /// is not calibration: the people's decisions on this uniform sample of production verdicts
    /// measure the precision and recall of each class, which is what the threshold should be
    /// set from. 0 turns the sample off.
    pub review_sample: f64,
    /// The person's fields that may be sent to a model: own attributes ([`OWN_FIELDS`]) and
    /// custom field keys. None by default.
    pub usable_fields: Vec<String>,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            classify_replies: false,
            generate_snippets: true,
            monthly_budget_micros: 10_000_000,
            confidence_threshold: 0.7,
            review_sample: 0.05,
            usable_fields: Vec::new(),
        }
    }
}

/// One invalid field of `settings.ai`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    /// The field's path under `settings.ai`, as an RFC 6901 pointer: `/monthly_budget_usd`,
    /// `/usable_fields/2`; empty for the object itself.
    pub pointer: String,
    /// What is wrong, for a person to read. It never repeats the value.
    pub detail: &'static str,
}

impl AiSettings {
    /// Reads and checks `settings.ai` as a workspace stores or sends it: an object whose fields
    /// are all optional; an absent value (`None` or `null`) is the defaults. Every invalid field
    /// is reported, unknown fields included.
    ///
    /// # Errors
    ///
    /// Every invalid field, in one list.
    pub fn parse(value: Option<&Value>) -> Result<Self, Vec<SettingsError>> {
        let mut settings = Self::default();
        let object = match value {
            None | Some(Value::Null) => return Ok(settings),
            Some(Value::Object(object)) => object,
            Some(_) => {
                return Err(vec![SettingsError {
                    pointer: String::new(),
                    detail: "the AI settings are an object",
                }]);
            }
        };
        let mut errors = Vec::new();
        let mut fail = |pointer: String, detail: &'static str| {
            errors.push(SettingsError { pointer, detail });
        };
        for (key, value) in object {
            let pointer = format!("/{}", key.replace('~', "~0").replace('/', "~1"));
            match key.as_str() {
                "classify_replies" => match value.as_bool() {
                    Some(on) => settings.classify_replies = on,
                    None => fail(pointer, "a boolean"),
                },
                "generate_snippets" => match value.as_bool() {
                    Some(on) => settings.generate_snippets = on,
                    None => fail(pointer, "a boolean"),
                },
                "monthly_budget_usd" => match budget(value) {
                    Some(micros) => settings.monthly_budget_micros = micros,
                    None => fail(
                        pointer,
                        "an amount of US dollars from 0 to 1,000,000 with at most two decimals",
                    ),
                },
                "confidence_threshold" => match value.as_f64() {
                    Some(threshold) if (0.0..=1.0).contains(&threshold) => {
                        settings.confidence_threshold = threshold;
                    }
                    _ => fail(pointer, "a number from 0 to 1"),
                },
                "review_sample" => match value.as_f64() {
                    Some(share) if (0.0..=1.0).contains(&share) => settings.review_sample = share,
                    _ => fail(pointer, "a share from 0 to 1"),
                },
                "usable_fields" => {
                    if let Some(fields) = usable_fields(value, &pointer, &mut fail) {
                        settings.usable_fields = fields;
                    }
                }
                _ => fail(pointer, "not an AI setting"),
            }
        }
        if errors.is_empty() {
            Ok(settings)
        } else {
            Err(errors)
        }
    }
}

/// A budget in micro-dollars from a JSON number with at most two decimals, read from its
/// decimal text so no binary fraction is ever rounded.
fn budget(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => parse_usd(&number.to_string(), 2).ok(),
        _ => None,
    }
}

/// The usable fields from a JSON array: own attributes or custom field keys, each once, at most
/// [`USABLE_FIELDS_MAX`]. Reports each invalid entry under `pointer`.
fn usable_fields(
    value: &Value,
    pointer: &str,
    fail: &mut impl FnMut(String, &'static str),
) -> Option<Vec<String>> {
    let Some(entries) = value.as_array() else {
        fail(pointer.to_owned(), "an array of field keys");
        return None;
    };
    if entries.len() > USABLE_FIELDS_MAX {
        fail(pointer.to_owned(), "at most 103 fields");
        return None;
    }
    let mut fields: Vec<String> = Vec::with_capacity(entries.len());
    let mut valid = true;
    for (index, entry) in entries.iter().enumerate() {
        let at = format!("{pointer}/{index}");
        let Some(key) = entry.as_str() else {
            fail(at, "a field key");
            valid = false;
            continue;
        };
        let known = OWN_FIELDS.contains(&key) || people::check_key(key).is_ok();
        if key == "email" {
            fail(at, "an address is never sent to a model");
            valid = false;
        } else if !known {
            fail(
                at,
                "given_name, family_name, company, or a custom field key: a lowercase letter followed by up to 63 lowercase letters, digits or underscores",
            );
            valid = false;
        } else if fields.iter().any(|field| field == key) {
            fail(at, "each field once");
            valid = false;
        } else {
            fields.push(key.to_owned());
        }
    }
    valid.then_some(fields)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::{
        AiSettings, CANARY_VERDICTS_MIN, CallOutcome, CallState, CanaryStats, ConfigError, Ending,
        ModelEntry, ModelRef, Notices, Phase, Price, Provider, ReviewReason, RouteError, Serve,
        Settlement, Usage, UseCase, Verdicts, admits, canary_phase, cost_micros, input_token_bound,
        month_of, notices, parse_usd, reservation_micros, review, route, sampled, serve, settle,
    };

    /// Claude Haiku 4.5's prices of 2026-10-01: $1 and $5 per million tokens.
    const HAIKU: Price = Price {
        input: 1_000_000,
        output: 5_000_000,
    };

    fn entry(text: &str) -> ModelEntry {
        text.parse().unwrap()
    }

    /// Amounts are read exactly from their decimal text, never through a binary fraction:
    /// `0.10` is 100,000 micro-dollars. Signs, exponents, more decimals than allowed, a bare
    /// point and amounts above a million dollars are refused, so a typo in a price or a budget
    /// never becomes a setting.
    #[test]
    fn dollar_amounts_are_read_exactly() {
        for (text, decimals, micros) in [
            ("0", 2, 0),
            ("10", 2, 10_000_000),
            ("12.5", 2, 12_500_000),
            ("0.01", 2, 10_000),
            ("0.10", 6, 100_000),
            ("0.000001", 6, 1),
            ("1000000", 2, 1_000_000_000_000),
        ] {
            assert_eq!(parse_usd(text, decimals), Ok(micros), "{text}");
        }
        for text in [
            "",
            "-1",
            "+1",
            "1e3",
            "1.",
            ".5",
            "0.001",
            "1,5",
            "1000000.01",
            "abc",
            "1.2.3",
        ] {
            assert_eq!(
                parse_usd(text, 2),
                Err(ConfigError::Amount(text.to_owned(), 2)),
                "{text}"
            );
        }
    }

    /// A catalogue entry names a provider, a model and its two prices, nothing more; anything
    /// else (an unknown provider, a missing price, a trailing flag such as a capability claim,
    /// which only the tested matrix may make) is refused, so the operator sees which entry is
    /// wrong.
    #[test]
    fn catalogue_entries_are_read_strictly() {
        assert_eq!(
            entry("anthropic/claude-haiku-4-5=1:5"),
            ModelEntry {
                model: ModelRef {
                    provider: Provider::Anthropic,
                    model: "claude-haiku-4-5".to_owned()
                },
                price: HAIKU,
            }
        );
        assert_eq!(entry("openai/llama3.1:8b=0:0").model.model, "llama3.1:8b");
        assert_eq!(entry("openai/gpt-6-luna=0.10:0.50").price.input, 100_000);
        for wrong in [
            "claude-haiku-4-5=1:5",
            "mistral/large=1:5",
            "anthropic/=1:5",
            "anthropic/claude=1",
            "anthropic/claude=1:5:strict",
            "anthropic/claude=1:5:fast",
            "anthropic/claude=-1:5",
        ] {
            assert!(wrong.parse::<ModelEntry>().is_err(), "{wrong} was accepted");
        }
    }

    /// Routing consults the capability matrix through the provider's wire, for every use case and
    /// every provider: a priced model the matrix marks strict is served; a priced model it marks
    /// not strict (a Claude model through chat completions, which Anthropic's compatibility layer
    /// does not constrain) and a priced model it never tested (a self-hosted one) are refused, as
    /// is a model the catalogue does not price. Every use case answers under a JSON schema, so an
    /// unconstrained model could never serve one, and the refusal stops the worker at start.
    #[test]
    fn routing_follows_the_capability_matrix() {
        let catalogue = [
            entry("anthropic/claude-haiku-4-5=1:5"),
            entry("openai/gpt-6-luna=0.10:0.50"),
            entry("openai/claude-haiku-4-5=1:5"),
            entry("openai/llama3.1=0:0"),
        ];
        for use_case in UseCase::iter() {
            for provider in Provider::iter() {
                let served = match provider {
                    Provider::Anthropic => "anthropic/claude-haiku-4-5",
                    Provider::OpenAi => "openai/gpt-6-luna",
                };
                let model = served.parse::<ModelRef>().unwrap();
                assert_eq!(route(use_case, &model, &catalogue).unwrap().model, model);
            }
            let refusal = |model: &str| {
                route(use_case, &model.parse::<ModelRef>().unwrap(), &catalogue).err()
            };
            assert!(matches!(
                refusal("openai/claude-haiku-4-5"),
                Some(RouteError::NotStrict { .. })
            ));
            assert!(matches!(
                refusal("openai/llama3.1"),
                Some(RouteError::Untested { .. })
            ));
            assert!(matches!(
                refusal("anthropic/claude-opus-5-5"),
                Some(RouteError::Unpriced { .. })
            ));
        }
    }

    /// Costs round up to the next micro-dollar, so settled spend never undercounts a bill; a
    /// reservation prices the input bound and the whole output limit, a bound on any bill of
    /// the call; the input bound is one token per byte plus the fixed allowance.
    #[test]
    fn costs_and_reservations_bound_the_bill() {
        assert_eq!(cost_micros(1_200, 150, HAIKU), 1_200 + 750);
        assert_eq!(
            cost_micros(
                1,
                0,
                Price {
                    input: 100_000,
                    output: 0
                }
            ),
            1,
            "a tenth of a micro-dollar rounds up"
        );
        assert_eq!(cost_micros(0, 0, HAIKU), 0);
        assert_eq!(
            cost_micros(u64::MAX, u64::MAX, HAIKU),
            u64::MAX,
            "saturates"
        );
        assert_eq!(input_token_bound(3_000), 3_000 + 1_024);
        assert_eq!(reservation_micros(4_024, 512, HAIKU), 4_024 + 2_560);
        // Any usage within the bounds costs at most the reservation.
        let reserved = reservation_micros(input_token_bound(3_000), 512, HAIKU);
        assert!(cost_micros(4_024, 512, HAIKU) <= reserved);
    }

    /// A reservation is admitted while spend, open reservations and the new one fit the budget,
    /// exactly up to it and not a micro-dollar beyond; a zero budget admits nothing.
    #[test]
    fn admission_keeps_spend_and_reservations_within_the_budget() {
        assert!(admits(10_000, 6_000, 3_000, 1_000));
        assert!(!admits(10_000, 6_000, 3_000, 1_001));
        assert!(!admits(0, 0, 0, 1));
        assert!(admits(0, 0, 0, 0));
        assert!(!admits(u64::MAX, u64::MAX, u64::MAX, 1), "no overflow");
    }

    /// The warning is due from 80 % of the budget settled, the exhaustion notice from 100 % or
    /// at the first refusal; nothing is due before anything was spent, except a refusal.
    #[test]
    fn notices_follow_the_thresholds() {
        let budget = 10_000_000;
        for (spent, refused, expected) in [
            (0, false, Notices::default()),
            (7_999_999, false, Notices::default()),
            (
                8_000_000,
                false,
                Notices {
                    warning: true,
                    exceeded: false,
                },
            ),
            (
                10_000_000,
                false,
                Notices {
                    warning: true,
                    exceeded: true,
                },
            ),
            (
                1_000,
                true,
                Notices {
                    warning: false,
                    exceeded: true,
                },
            ),
        ] {
            assert_eq!(
                notices(spent, budget, refused),
                expected,
                "{spent} {refused}"
            );
        }
        assert_eq!(notices(0, 0, false), Notices::default());
        assert_eq!(
            notices(5, 0, false),
            Notices {
                warning: true,
                exceeded: true
            },
            "spend under a budget lowered to zero"
        );
    }

    /// The month of an instant is its UTC month, at both edges of a month and across a year:
    /// the last microsecond of January is January, the first of February is February, so a call
    /// reserved then settles against the month it was reserved in.
    #[test]
    fn months_roll_over_at_utc_midnight() {
        for (at, month) in [
            ("2026-01-31T23:59:59.999999Z", "2026-01-01"),
            ("2026-02-01T00:00:00Z", "2026-02-01"),
            ("2026-12-31T23:59:59Z", "2026-12-01"),
            ("2027-01-01T00:00:00Z", "2027-01-01"),
            ("2026-03-01T00:30:00+01:00", "2026-02-01"),
        ] {
            assert_eq!(
                month_of(at.parse().unwrap()).to_string(),
                month,
                "{at} belongs to {month}"
            );
        }
    }

    /// Every way a call can end has its settlement: answers are charged their usage (their
    /// reservation without one), errors the provider does not bill nothing, lost answers,
    /// deadlines and abandoned runs their reservation, and a call never sent is released. A new
    /// ending fails here until its settlement is decided.
    #[test]
    fn every_ending_is_settled() {
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 100,
        };
        let reserved = 9_999;
        let used = cost_micros(1_000, 100, HAIKU);
        for ending in Ending::iter() {
            let expected = match ending {
                Ending::Completed => (CallState::Settled, Some(CallOutcome::Completed), used),
                Ending::Refused => (CallState::Settled, Some(CallOutcome::Refused), used),
                Ending::Truncated => (CallState::Settled, Some(CallOutcome::Truncated), used),
                Ending::InvalidOutput => {
                    (CallState::Settled, Some(CallOutcome::InvalidOutput), used)
                }
                Ending::NotBilled => (CallState::Settled, Some(CallOutcome::ProviderError), 0),
                Ending::Lost => (
                    CallState::Settled,
                    Some(CallOutcome::ProviderError),
                    reserved,
                ),
                Ending::TimedOut => (CallState::Settled, Some(CallOutcome::Timeout), reserved),
                Ending::NotSent => (CallState::Released, None, 0),
                Ending::Interrupted => (
                    CallState::Interrupted,
                    Some(CallOutcome::Interrupted),
                    reserved,
                ),
            };
            let Settlement {
                state,
                outcome,
                charged,
                usage: recorded,
            } = settle(ending, Some(usage), HAIKU, reserved);
            assert_eq!((state, outcome, charged), expected, "{ending:?}");
            let answered = matches!(
                ending,
                Ending::Completed | Ending::Refused | Ending::Truncated | Ending::InvalidOutput
            );
            assert_eq!(recorded.is_some(), answered, "{ending:?} records usage");
            if answered {
                assert_eq!(
                    settle(ending, None, HAIKU, reserved).charged,
                    reserved,
                    "{ending:?} without usage figures is charged its reservation"
                );
            }
        }
    }

    /// Absent settings are the defaults: classification off, snippets on, ten dollars a month,
    /// a 0.7 threshold, a 5 % review sample and no usable field. A partial object fills the rest
    /// from the defaults, and a full one is read exactly, its budget from its decimal text.
    #[test]
    fn settings_default_and_round_trip() {
        let defaults = AiSettings::parse(None).unwrap();
        assert_eq!(defaults, AiSettings::default());
        assert!(!defaults.classify_replies);
        assert!(defaults.generate_snippets);
        assert_eq!(defaults.monthly_budget_micros, 10_000_000);
        assert!((defaults.review_sample - 0.05).abs() < f64::EPSILON);
        assert_eq!(AiSettings::parse(Some(&json!(null))).unwrap(), defaults);
        let partial = AiSettings::parse(Some(&json!({"classify_replies": true}))).unwrap();
        assert!(partial.classify_replies);
        assert_eq!(partial.monthly_budget_micros, 10_000_000);
        let full = AiSettings::parse(Some(&json!({
            "classify_replies": true,
            "generate_snippets": false,
            "monthly_budget_usd": 12.5,
            "confidence_threshold": 0.85,
            "review_sample": 0,
            "usable_fields": ["given_name", "company", "industry"],
        })))
        .unwrap();
        assert_eq!(full.monthly_budget_micros, 12_500_000);
        assert!(full.review_sample.abs() < f64::EPSILON);
        assert_eq!(full.usable_fields, ["given_name", "company", "industry"]);
    }

    /// Below the threshold a verdict is reviewed for its low confidence, whatever the draw; at or
    /// above it, it is applied unless the draw falls in the sample share, and then it is
    /// reviewed as a sample. Every reason has a case that produces it, so a new reason fails here
    /// until it has one.
    #[test]
    fn a_verdict_is_applied_reviewed_or_sampled() {
        for reason in ReviewReason::iter() {
            let (confidence, sample, draw) = match reason {
                ReviewReason::LowConfidence => (0.69, 0.0, u64::MAX),
                ReviewReason::Sample => (0.95, 0.05, 0),
            };
            assert_eq!(review(confidence, 0.7, sample, draw), Some(reason));
        }
        assert_eq!(review(0.1, 0.7, 1.0, 0), Some(ReviewReason::LowConfidence));
        assert_eq!(
            review(0.7, 0.7, 0.0, 0),
            None,
            "the threshold itself applies"
        );
        assert_eq!(review(0.99, 0.7, 0.05, u64::MAX), None);
        assert_eq!(review(0.99, 0.7, 1.0, u64::MAX), Some(ReviewReason::Sample));
    }

    /// The sample takes the configured share of uniformly spread draws, to within one draw in ten
    /// thousand; a share of 0 never samples and a share of 1 always does, at both ends of the
    /// draw's range.
    #[test]
    fn the_sample_takes_its_share() {
        let steps: u64 = 10_000;
        let draws: Vec<u64> = (0..steps)
            .map(|step| (step * (u64::from(u32::MAX) / steps)) << 32)
            .collect();
        for (share, expected) in [(0.0, 0), (0.05, 500), (0.5, 5_000), (1.0, 10_000)] {
            let taken = draws.iter().filter(|&&draw| sampled(share, draw)).count();
            assert!(
                taken.abs_diff(expected) <= 1,
                "{share}: {taken} of {steps}, expected {expected}"
            );
        }
        for draw in [0, u64::MAX] {
            assert!(!sampled(0.0, draw));
            assert!(sampled(1.0, draw));
        }
    }

    /// A serving canary takes 5 % of uniformly spread calls (to within one in ten thousand) as a
    /// canary and leaves the rest to the current prompt; a rolled-back one takes none and a
    /// promoted one takes every call as the use case's prompt. Every phase has its expected
    /// choice for a draw inside and outside the share, so a new phase fails here until it has
    /// one.
    #[test]
    fn the_canary_serves_its_share_while_it_is_measured() {
        for phase in Phase::iter() {
            let (inside, outside) = match phase {
                Phase::Serving => (Serve::Canary, Serve::Current),
                Phase::RolledBack => (Serve::Current, Serve::Current),
                Phase::Promoted => (Serve::Promoted, Serve::Promoted),
            };
            assert_eq!(serve(phase, 0), inside, "{phase:?}");
            assert_eq!(serve(phase, u64::MAX), outside, "{phase:?}");
        }
        let steps: u64 = 10_000;
        let canaries = (0..steps)
            .map(|step| (step * (u64::from(u32::MAX) / steps)) << 32)
            .filter(|&draw| serve(Phase::Serving, draw) == Serve::Canary)
            .count();
        assert!(canaries.abs_diff(500) <= 1, "{canaries} of {steps}");
    }

    /// The guard, for every phase: a promotion is final; nothing is judged before the first
    /// verdict or on fewer than the minimum of verdicts, however bad; a review rate more than the
    /// margin above the current prompt's rolls the canary back, even after its period, while one
    /// at the margin does not; a canary within the margin is promoted once its period has passed
    /// and serves until then.
    #[test]
    fn the_guard_rolls_back_a_canary_whose_review_rate_rises() {
        let first = jiff::Timestamp::from_second(1_790_000_000).unwrap();
        let later = |hours: i64| {
            first
                .checked_add(jiff::SignedDuration::from_hours(hours))
                .unwrap()
        };
        let verdicts = |verdicts: u64, reviews: u64| Verdicts { verdicts, reviews };
        let stats = |canary: Verdicts, promoted: bool| CanaryStats {
            first: Some(first),
            canary,
            current: verdicts(1_000, 100),
            promoted,
        };
        let margin = 5;
        for phase in Phase::iter() {
            let (case, now) = match phase {
                Phase::Serving => (stats(verdicts(100, 12), false), later(23)),
                Phase::RolledBack => (stats(verdicts(100, 16), false), later(2)),
                Phase::Promoted => (stats(verdicts(100, 12), false), later(24)),
            };
            assert_eq!(canary_phase(&case, now, margin), phase, "{case:?}");
        }
        assert_eq!(
            canary_phase(&stats(verdicts(100, 90), true), later(1), margin),
            Phase::Promoted
        );
        assert_eq!(
            canary_phase(&CanaryStats::default(), later(48), margin),
            Phase::Serving
        );
        let few = verdicts(CANARY_VERDICTS_MIN - 1, CANARY_VERDICTS_MIN - 1);
        assert_eq!(
            canary_phase(&stats(few, false), later(48), margin),
            Phase::Serving
        );
        assert_eq!(
            canary_phase(&stats(verdicts(100, 16), false), later(48), margin),
            Phase::RolledBack
        );
        assert_eq!(
            canary_phase(&stats(verdicts(100, 15), false), later(2), margin),
            Phase::Serving,
            "a rise of exactly the margin is within it"
        );
        assert_eq!(
            canary_phase(&stats(verdicts(100, 16), false), later(2), 10),
            Phase::Serving
        );
        assert!(verdicts(0, 0).rate().abs() < f64::EPSILON);
    }

    /// Every invalid field is reported in one answer with its pointer: wrong types, a budget
    /// with fractions of a cent or above a million dollars, a threshold outside 0 to 1, an
    /// unknown setting, the address, a malformed key and a repeated field.
    #[test]
    fn settings_report_every_invalid_field() {
        let errors = AiSettings::parse(Some(&json!({
            "classify_replies": "yes",
            "monthly_budget_usd": 10.005,
            "confidence_threshold": 1.5,
            "temperature": 0.2,
            "usable_fields": ["email", "Company", "company", "company"],
        })))
        .unwrap_err();
        let mut pointers: Vec<&str> = errors.iter().map(|error| error.pointer.as_str()).collect();
        pointers.sort_unstable();
        assert_eq!(
            pointers,
            [
                "/classify_replies",
                "/confidence_threshold",
                "/monthly_budget_usd",
                "/temperature",
                "/usable_fields/0",
                "/usable_fields/1",
                "/usable_fields/3",
            ]
        );
        for wrong in [
            json!({"monthly_budget_usd": -1}),
            json!({"monthly_budget_usd": 1_000_001}),
            json!({"monthly_budget_usd": "10"}),
            json!({"generate_snippets": 1}),
            json!({"usable_fields": "company"}),
            json!([]),
        ] {
            assert!(
                AiSettings::parse(Some(&wrong)).is_err(),
                "{wrong} was accepted"
            );
        }
        let many: Vec<String> = (0..104).map(|index| format!("field_{index}")).collect();
        assert!(AiSettings::parse(Some(&json!({ "usable_fields": many }))).is_err());
    }
}
