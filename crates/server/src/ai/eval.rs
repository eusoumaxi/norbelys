//! The evaluation of the AI use cases against their golden sets.
//!
//! Each use case has a golden set of at least 50 labelled cases in `crates/ai/evals/`: inbound
//! replies with their classification and sentiment, snippet requests with what the snippets
//! must and must not mention, contacts with their risk. No case holds real personal data. An
//! evaluation sends every case through the use case's production request (its current prompt,
//! input, schema, output limit and checks) to a model, and measures:
//!
//! - for classification and hints, the precision and the recall of every class (and of every
//!   sentiment): self-reported confidence is not calibration, so quality is measured per class;
//! - for snippets, the share of cases whose snippets mention what they must, and the share that
//!   leak nothing they must not;
//! - the violations (answers that failed the schema or the checks) and the failures (calls that
//!   brought no answer).
//!
//! A run fails on any violation or failure, and on any metric more than two points below the
//! committed baseline of its use case (`crates/ai/evals/<use case>.baseline.json`): the
//! measurement of the prompt and the model in production. A prompt or model change ships only
//! when its run passes, and the passing run is then recorded as the new baseline. Without a
//! baseline, only violations and failures fail.
//!
//! The run against a real model is an ignored test, since it costs money and needs a provider
//! key; the decision logic and the harness are tested here against the fake provider.
//!
//! # In CI, and the first baselines
//!
//! The private infrastructure repository owns the manual `AI evaluation` workflow and its
//! provider credentials. Public CI uses fake providers; paid evaluations run only when
//! explicitly dispatched against a committed revision. Baselines begin with a real run: run the
//! workflow by hand with `write_baseline` ticked (it sets `AI_EVAL_WRITE_BASELINE` and uploads
//! the measured `*.baseline.json` files as an artifact), or run the test locally with
//! `AI_EVAL_WRITE_BASELINE=1` and a key, which writes them in place; then commit them under
//! `crates/ai/evals/`. A change that passes is recorded the same way as the new baseline.

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures_util::StreamExt as _;
use norbelys_ai::{Client, Message, Outcome, Output, Request};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::prompts::{Prompt, canary, current};
use super::{classify, hints, snippets};
use crate::domain::ai::UseCase;

/// The most a metric may fall below its baseline, in points of a share (0.02 is two points).
const TOLERANCE: f64 = 0.02;
/// Calls in flight at once during an evaluation.
const CONCURRENCY: usize = 4;

const CLASSIFICATION: &str = include_str!("../../../ai/evals/classification.jsonl");
const SNIPPETS: &str = include_str!("../../../ai/evals/snippets.jsonl");
const HINTS: &str = include_str!("../../../ai/evals/hints.jsonl");

/// One labelled inbound reply.
#[derive(Debug, Clone, Deserialize)]
struct ClassificationCase {
    id: String,
    from: Option<String>,
    subject: Option<String>,
    auto_submitted: Option<String>,
    in_reply_to: Option<String>,
    body: String,
    expected: ClassificationLabel,
}

#[derive(Debug, Clone, Deserialize)]
struct ClassificationLabel {
    classification: String,
    sentiment: String,
}

/// One labelled snippet request.
#[derive(Debug, Clone, Deserialize)]
struct SnippetsCase {
    id: String,
    instructions: String,
    names: Vec<String>,
    fields: Map<String, Value>,
    usable_fields: Vec<String>,
    expect: SnippetsLabel,
}

#[derive(Debug, Clone, Deserialize)]
struct SnippetsLabel {
    /// Texts the snippets must contain, ignoring case.
    mentions: Vec<String>,
    /// Texts they must not contain, ignoring case: unusable fields, injected instructions.
    forbids: Vec<String>,
}

/// One labelled contact.
#[derive(Debug, Clone, Deserialize)]
struct HintsCase {
    id: String,
    fields: Map<String, Value>,
    expected: HintsLabel,
}

#[derive(Debug, Clone, Deserialize)]
struct HintsLabel {
    risk: String,
}

/// The cases of a golden set, one JSON object per line.
fn cases<T: DeserializeOwned>(jsonl: &str) -> Vec<T> {
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a golden case reads"))
        .collect()
}

/// What one evaluated call came to.
enum Got<T> {
    /// A valid answer.
    Answer(T),
    /// A refusal or a truncated answer: billed, but no answer to score.
    Unusable,
    /// An answer that failed the schema or the checks.
    Violation,
    /// No answer at all.
    Failed,
}

/// One case's call, made as production makes it, without the ledger.
async fn ask<T>(
    client: &Client,
    prompt: &Prompt,
    id: &str,
    messages: Vec<Message>,
    schema: Value,
    max_tokens: u32,
    check: impl FnOnce(&Value) -> Result<T, String>,
) -> Got<T> {
    let request = Request {
        call_id: format!("eval-{}-{id}", prompt.use_case.as_str()),
        system: prompt.text.to_owned(),
        messages,
        max_tokens,
        temperature: None,
        schema: Some(schema),
    };
    match client.complete(&request).await {
        Ok(completion) => match completion.outcome {
            Outcome::Completed(Output::Json(value)) => {
                check(&value).map_or(Got::Violation, Got::Answer)
            }
            Outcome::Completed(Output::Text(_)) | Outcome::InvalidOutput { .. } => Got::Violation,
            Outcome::Refused { .. } | Outcome::Truncated { .. } => Got::Unusable,
        },
        Err(_) => Got::Failed,
    }
}

/// A measurement of one use case against its golden set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The use case.
    pub use_case: String,
    /// The prompt's id and version.
    pub prompt: String,
    /// The model, `provider/model`.
    pub model: String,
    /// The cases evaluated.
    pub cases: usize,
    /// Answers that failed the schema or the checks.
    pub violations: usize,
    /// Calls that brought no answer.
    pub failures: usize,
    /// Each metric by name, as a share from 0 to 1: `classification/out_of_office/recall`.
    pub metrics: BTreeMap<String, f64>,
}

/// Counts of one evaluation, before the metrics.
#[derive(Default)]
struct Tally {
    violations: usize,
    failures: usize,
}

impl Tally {
    /// The answer of `got`, counting what is not one.
    fn answer<T>(&mut self, got: Got<T>) -> Option<T> {
        match got {
            Got::Answer(answer) => Some(answer),
            Got::Unusable => None,
            Got::Violation => {
                self.violations += 1;
                None
            }
            Got::Failed => {
                self.failures += 1;
                None
            }
        }
    }
}

/// Evaluates `use_case` on `client`, the model `model`, with the prompt a change would ship: the
/// use case's canary when one is declared (a new version must pass before it serves any call),
/// else its current prompt.
async fn evaluate(use_case: UseCase, client: &Client, model: &str) -> Report {
    let prompt = canary(use_case).unwrap_or_else(|| current(use_case));
    let mut tally = Tally::default();
    let (count, metrics) = match use_case {
        UseCase::Classification => {
            let golden: Vec<ClassificationCase> = cases(CLASSIFICATION);
            let answers: Vec<Got<classify::Verdict>> = futures_util::stream::iter(&golden)
                .map(|case| {
                    ask(
                        client,
                        prompt,
                        &case.id,
                        classify::messages(&inbound(case)),
                        classify::schema(),
                        classify::MAX_TOKENS,
                        classify::check,
                    )
                })
                .buffered(CONCURRENCY)
                .collect()
                .await;
            let mut classes = Vec::with_capacity(golden.len());
            let mut sentiments = Vec::with_capacity(golden.len());
            for (case, got) in golden.iter().zip(answers) {
                let verdict = tally.answer(got);
                classes.push((
                    case.expected.classification.clone(),
                    verdict
                        .as_ref()
                        .map(|verdict| verdict.classification.as_str().to_owned()),
                ));
                sentiments.push((
                    case.expected.sentiment.clone(),
                    verdict.map(|verdict| verdict.sentiment.as_str().to_owned()),
                ));
            }
            let mut metrics = per_class(
                "classification",
                &["human_reply", "auto_reply", "out_of_office", "unknown"],
                &classes,
            );
            metrics.extend(per_class(
                "sentiment",
                &["positive", "neutral", "negative"],
                &sentiments,
            ));
            (golden.len(), metrics)
        }
        UseCase::Snippets => {
            let golden: Vec<SnippetsCase> = cases(SNIPPETS);
            let answers: Vec<_> = futures_util::stream::iter(&golden)
                .map(|case| {
                    let request = snippets::SnippetsRequest {
                        instructions: &case.instructions,
                        names: &case.names,
                        fields: &case.fields,
                        deadline: None,
                    };
                    ask(
                        client,
                        prompt,
                        &case.id,
                        snippets::messages(&request, &case.usable_fields),
                        snippets::schema(&case.names),
                        snippets::MAX_TOKENS,
                        |value| snippets::check(value, &case.names),
                    )
                })
                .buffered(CONCURRENCY)
                .collect()
                .await;
            let mut mentioned = 0_u32;
            let mut clean = 0_u32;
            for (case, got) in golden.iter().zip(answers) {
                let Some(written) = tally.answer(got) else {
                    continue;
                };
                let text = written
                    .values()
                    .map(|snippet| snippet.to_lowercase())
                    .collect::<Vec<_>>()
                    .join("\n");
                let has = |needle: &String| text.contains(&needle.to_lowercase());
                mentioned += u32::from(case.expect.mentions.iter().all(has));
                clean += u32::from(!case.expect.forbids.iter().any(has));
            }
            let total = f64::from(u32::try_from(golden.len()).unwrap_or(u32::MAX).max(1));
            let metrics = BTreeMap::from([
                ("snippets/mentions".to_owned(), f64::from(mentioned) / total),
                ("snippets/exclusions".to_owned(), f64::from(clean) / total),
            ]);
            (golden.len(), metrics)
        }
        UseCase::Hints => {
            let golden: Vec<HintsCase> = cases(HINTS);
            let answers: Vec<Got<hints::Hint>> = futures_util::stream::iter(&golden)
                .map(|case| {
                    ask(
                        client,
                        prompt,
                        &case.id,
                        hints::messages(&case.fields),
                        hints::schema(),
                        hints::MAX_TOKENS,
                        hints::check,
                    )
                })
                .buffered(CONCURRENCY)
                .collect()
                .await;
            let pairs: Vec<(String, Option<String>)> = golden
                .iter()
                .zip(answers)
                .map(|(case, got)| {
                    (
                        case.expected.risk.clone(),
                        tally.answer(got).map(|hint| hint.risk.as_str().to_owned()),
                    )
                })
                .collect();
            (
                golden.len(),
                per_class("risk", &["low", "medium", "high"], &pairs),
            )
        }
    };
    Report {
        use_case: use_case.as_str().to_owned(),
        prompt: prompt.id.clone(),
        model: model.to_owned(),
        cases: count,
        violations: tally.violations,
        failures: tally.failures,
        metrics,
    }
}

/// The inbound message of a classification case.
fn inbound(case: &ClassificationCase) -> classify::Inbound<'_> {
    classify::Inbound {
        from: case.from.as_deref(),
        subject: case.subject.as_deref(),
        auto_submitted: case.auto_submitted.as_deref(),
        in_reply_to: case.in_reply_to.as_deref(),
        body: &case.body,
    }
}

/// The precision and the recall of every class of `dimension`, from (expected, predicted)
/// pairs; a case without a prediction is a miss of its expected class. A class nothing was
/// predicted as has a precision of 1 (it made no false claim), and one with no case a recall of
/// 1, so only real errors lower a metric.
fn per_class(
    dimension: &str,
    classes: &[&str],
    pairs: &[(String, Option<String>)],
) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    for class in classes {
        let predicted = |pair: &&(String, Option<String>)| pair.1.as_deref() == Some(*class);
        let expected = |pair: &&(String, Option<String>)| pair.0 == *class;
        let hits = pairs
            .iter()
            .filter(|pair| predicted(pair) && expected(pair))
            .count();
        let claimed = pairs.iter().filter(predicted).count();
        let actual = pairs.iter().filter(expected).count();
        let share = |part: usize, whole: usize| {
            if whole == 0 {
                1.0
            } else {
                f64::from(u32::try_from(part).unwrap_or(u32::MAX))
                    / f64::from(u32::try_from(whole).unwrap_or(u32::MAX))
            }
        };
        metrics.insert(
            format!("{dimension}/{class}/precision"),
            share(hits, claimed),
        );
        metrics.insert(format!("{dimension}/{class}/recall"), share(hits, actual));
    }
    metrics
}

/// Every metric of `baseline` that `current` lost by more than [`TOLERANCE`], or lost
/// altogether, described for a person.
fn regressions(baseline: &Report, current: &Report) -> Vec<String> {
    baseline
        .metrics
        .iter()
        .filter_map(|(name, before)| match current.metrics.get(name) {
            None => Some(format!("{name} is no longer measured")),
            Some(now) if before - now > TOLERANCE + f64::EPSILON => Some(format!(
                "{name} fell from {before:.3} to {now:.3}, more than {TOLERANCE} below the baseline"
            )),
            Some(_) => None,
        })
        .collect()
}

/// Whether `current` may ship: no violation, no failure, and no regression from `baseline`
/// when there is one.
///
/// # Errors
///
/// Every reason it may not, for a person.
fn verdict(baseline: Option<&Report>, current: &Report) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();
    if current.violations > 0 {
        reasons.push(format!(
            "{} answers failed the schema or the checks",
            current.violations
        ));
    }
    if current.failures > 0 {
        reasons.push(format!("{} calls brought no answer", current.failures));
    }
    if let Some(baseline) = baseline {
        reasons.extend(regressions(baseline, current));
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

/// The committed baseline file of `use_case`.
fn baseline_path(use_case: UseCase) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../ai/evals")
        .join(format!("{}.baseline.json", use_case.as_str()))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::super::Ai;
    use super::super::fake::{Fake, MODEL, Reply, json_answer, last_user_turn};
    use super::{
        CLASSIFICATION, ClassificationCase, HINTS, HintsCase, Report, SNIPPETS, SnippetsCase,
        baseline_path, cases, evaluate, inbound, per_class, regressions, verdict,
    };
    use crate::ai::{classify, hints, snippets};
    use crate::domain::ai::UseCase;

    /// Every golden set reads, holds at least 50 cases with distinct ids, and labels each case
    /// with values its use case's schema allows; every snippet request is one the use case
    /// would serve. A malformed or thin golden set would make every evaluation meaningless.
    #[test]
    fn golden_sets_are_labelled_and_large_enough() {
        let classification: Vec<ClassificationCase> = cases(CLASSIFICATION);
        let snippet_cases: Vec<SnippetsCase> = cases(SNIPPETS);
        let hint_cases: Vec<HintsCase> = cases(HINTS);
        let distinct = |ids: Vec<&str>| {
            let count = ids.len();
            let mut ids = ids;
            ids.sort_unstable();
            ids.dedup();
            ids.len() == count
        };
        assert!(classification.len() >= 50);
        assert!(snippet_cases.len() >= 50);
        assert!(hint_cases.len() >= 50);
        assert!(distinct(
            classification.iter().map(|case| case.id.as_str()).collect()
        ));
        assert!(distinct(
            snippet_cases.iter().map(|case| case.id.as_str()).collect()
        ));
        assert!(distinct(
            hint_cases.iter().map(|case| case.id.as_str()).collect()
        ));
        for case in &classification {
            let answer = json!({
                "classification": case.expected.classification,
                "sentiment": case.expected.sentiment,
                "confidence": 1,
                "reasons": [],
            });
            assert!(classify::check(&answer).is_ok(), "{}", case.id);
        }
        for case in &snippet_cases {
            let request = snippets::SnippetsRequest {
                instructions: &case.instructions,
                names: &case.names,
                fields: &case.fields,
                deadline: None,
            };
            assert!(snippets::usable(&request), "{}", case.id);
        }
        for case in &hint_cases {
            let answer = json!({"risk": case.expected.risk, "reasons": []});
            assert!(hints::check(&answer).is_ok(), "{}", case.id);
        }
    }

    /// Precision and recall per class: a miss lowers the expected class's recall, a wrong claim
    /// the claimed class's precision, and a class nobody claimed or expected keeps 1.
    #[test]
    fn precision_and_recall_count_per_class() {
        let pair = |expected: &str, predicted: Option<&str>| {
            (expected.to_owned(), predicted.map(str::to_owned))
        };
        let pairs = [
            pair("a", Some("a")),
            pair("a", Some("b")),
            pair("a", None),
            pair("b", Some("b")),
        ];
        let metrics = per_class("x", &["a", "b", "c"], &pairs);
        assert_eq!(metrics["x/a/precision"], 1.0);
        assert!((metrics["x/a/recall"] - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(metrics["x/b/precision"], 0.5);
        assert_eq!(metrics["x/b/recall"], 1.0);
        assert_eq!(metrics["x/c/precision"], 1.0);
        assert_eq!(metrics["x/c/recall"], 1.0);
    }

    fn report(metric: f64, violations: usize) -> Report {
        Report {
            use_case: "classification".to_owned(),
            prompt: "classification/reply-v1".to_owned(),
            model: MODEL.to_owned(),
            cases: 50,
            violations,
            failures: 0,
            metrics: BTreeMap::from([("classification/unknown/recall".to_owned(), metric)]),
        }
    }

    /// A metric may fall two points below its baseline and no more; any violation fails a run,
    /// with or without a baseline; a metric that disappears is a regression too.
    #[test]
    fn the_verdict_allows_two_points_and_no_violation() {
        let baseline = report(0.90, 0);
        assert!(verdict(Some(&baseline), &report(0.88, 0)).is_ok());
        assert!(verdict(Some(&baseline), &report(0.95, 0)).is_ok());
        let failed = verdict(Some(&baseline), &report(0.879, 0)).unwrap_err();
        assert!(
            failed[0].contains("classification/unknown/recall"),
            "{failed:?}"
        );
        assert!(
            verdict(None, &report(0.10, 0)).is_ok(),
            "no baseline, no comparison"
        );
        assert!(verdict(None, &report(0.99, 1)).is_err());
        let mut gone = report(0.9, 0);
        gone.metrics.clear();
        assert_eq!(regressions(&baseline, &gone).len(), 1);
    }

    /// The labels of the classification cases, by the input production sends for each.
    fn labels() -> HashMap<String, (String, String)> {
        cases::<ClassificationCase>(CLASSIFICATION)
            .into_iter()
            .map(|case| {
                let input = classify::messages(&inbound(&case))[0].content.clone();
                (
                    input,
                    (case.expected.classification, case.expected.sentiment),
                )
            })
            .collect()
    }

    /// A fake model that answers each classification case with its label, except the classes in
    /// `confused`, which it calls `unknown`, and the case containing `broken`, which it answers
    /// outside the schema.
    async fn classifier(confused: &'static [&'static str], broken: Option<&'static str>) -> Fake {
        let labels = Arc::new(labels());
        Fake::start(move |request| {
            let input = last_user_turn(request);
            if broken.is_some_and(|marker| input.contains(marker)) {
                return json_answer(&json!({"classification": "bounce"}));
            }
            let (classification, sentiment) = labels
                .get(&input)
                .cloned()
                .unwrap_or(("unknown".to_owned(), "neutral".to_owned()));
            let classification = if confused.contains(&classification.as_str()) {
                "unknown".to_owned()
            } else {
                classification
            };
            json_answer(&json!({
                "classification": classification,
                "sentiment": sentiment,
                "confidence": 0.9,
                "reasons": ["Wording"],
            }))
        })
        .await
    }

    /// The whole classification golden set runs through the production request: a model that
    /// answers every label scores 1 everywhere and passes against itself; one that mistakes
    /// out-of-office notices for unknown fails against that baseline on their recall; one that
    /// answers a single case outside the schema fails on that violation alone.
    #[tokio::test]
    async fn the_evaluation_passes_a_good_model_and_fails_a_regressed_one() {
        let exact = classifier(&[], None).await;
        let ai = Ai::for_evaluation(&exact.args(5)).unwrap();
        let client = &ai.route(UseCase::Classification).unwrap().client;
        let baseline = evaluate(UseCase::Classification, client, MODEL).await;
        assert_eq!(
            baseline.cases,
            cases::<ClassificationCase>(CLASSIFICATION).len()
        );
        assert_eq!((baseline.violations, baseline.failures), (0, 0));
        assert!(
            baseline
                .metrics
                .values()
                .all(|metric| (*metric - 1.0).abs() < 1e-9)
        );
        assert!(verdict(Some(&baseline), &baseline).is_ok());

        let confused = classifier(&["out_of_office"], None).await;
        let ai = Ai::for_evaluation(&confused.args(5)).unwrap();
        let client = &ai.route(UseCase::Classification).unwrap().client;
        let regressed = evaluate(UseCase::Classification, client, MODEL).await;
        let reasons = verdict(Some(&baseline), &regressed).unwrap_err();
        assert!(
            reasons
                .iter()
                .any(|reason| reason.starts_with("classification/out_of_office/recall")),
            "{reasons:?}"
        );

        let broken = classifier(&[], Some("Tuesday at 3pm")).await;
        let ai = Ai::for_evaluation(&broken.args(5)).unwrap();
        let client = &ai.route(UseCase::Classification).unwrap().client;
        let violated = evaluate(UseCase::Classification, client, MODEL).await;
        assert_eq!(violated.violations, 1);
        assert!(verdict(None, &violated).is_err());
    }

    /// The snippets and hints golden sets run through their production requests too: a model
    /// that writes every required mention and labels every risk scores 1 on each metric.
    #[tokio::test]
    async fn snippets_and_hints_are_measured() {
        let mentions: HashMap<String, Vec<String>> = cases::<SnippetsCase>(SNIPPETS)
            .into_iter()
            .map(|case| {
                let request = snippets::SnippetsRequest {
                    instructions: &case.instructions,
                    names: &case.names,
                    fields: &case.fields,
                    deadline: None,
                };
                let input = snippets::messages(&request, &case.usable_fields)[0]
                    .content
                    .clone();
                (input, case.expect.mentions)
            })
            .collect();
        let risks: HashMap<String, String> = cases::<HintsCase>(HINTS)
            .into_iter()
            .map(|case| {
                let input = hints::messages(&case.fields)[0].content.clone();
                (input, case.expected.risk)
            })
            .collect();
        let (mentions, risks) = (Arc::new(mentions), Arc::new(risks));
        let fake = Fake::start(move |request| {
            let input = last_user_turn(request);
            if let Some(risk) = risks.get(&input) {
                return json_answer(&json!({"risk": risk, "reasons": ["Fields"]}));
            }
            let Some(wanted) = mentions.get(&input) else {
                return Reply::Status {
                    status: 400,
                    retry_after: None,
                };
            };
            let names = request["response_format"]["json_schema"]["schema"]["properties"]
                ["snippets"]["required"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let text = if wanted.is_empty() {
                "A neutral line.".to_owned()
            } else {
                wanted.join(" and ")
            };
            let snippets: serde_json::Map<String, serde_json::Value> = names
                .iter()
                .filter_map(|name| name.as_str())
                .map(|name| (name.to_owned(), json!(text)))
                .collect();
            json_answer(&json!({ "snippets": snippets }))
        })
        .await;
        let ai = Ai::for_evaluation(&fake.args(5)).unwrap();
        for use_case in [UseCase::Snippets, UseCase::Hints] {
            let client = &ai.route(use_case).unwrap().client;
            let measured = evaluate(use_case, client, MODEL).await;
            assert_eq!(
                (measured.violations, measured.failures),
                (0, 0),
                "{use_case:?}"
            );
            assert!(
                measured
                    .metrics
                    .values()
                    .all(|metric| (*metric - 1.0).abs() < 1e-9),
                "{measured:?}"
            );
        }
    }

    /// The run against the deployment's configured models, on request (see the ignore reason):
    /// every use case is evaluated, compared with its committed baseline when there is one, and
    /// recorded as the new baseline when it passes and `AI_EVAL_WRITE_BASELINE` is set.
    #[tokio::test]
    #[ignore = "calls the configured AI providers with real keys and costs money: run with ANTHROPIC_API_KEY or OPENAI_API_KEY set, `cargo test -p norbelys-server ai::eval::tests::against_the_configured_models -- --ignored`; without a committed baseline only violations and failures fail, and AI_EVAL_WRITE_BASELINE=1 records a passing run as the baseline"]
    async fn against_the_configured_models() {
        let (args, write_baseline) =
            crate::config::test_eval_settings().expect("the AI variables read");
        let ai = Ai::for_evaluation(&args).expect("the AI configuration is valid");
        let mut problems = Vec::new();
        for use_case in UseCase::iter() {
            let route = ai.route(use_case).unwrap_or_else(|| {
                panic!(
                    "{} has no provider key: set ANTHROPIC_API_KEY or OPENAI_API_KEY",
                    use_case.as_str()
                )
            });
            let report = evaluate(use_case, &route.client, &route.entry.model.to_string()).await;
            let path = baseline_path(use_case);
            let baseline: Option<Report> = std::fs::read(&path)
                .ok()
                .map(|bytes| serde_json::from_slice(&bytes).expect("the baseline reads"));
            match verdict(baseline.as_ref(), &report) {
                Ok(()) if write_baseline => std::fs::write(
                    &path,
                    format!("{}\n", serde_json::to_string_pretty(&report).expect("JSON")),
                )
                .expect("the baseline is written"),
                Ok(()) => {}
                Err(reasons) => problems.extend(
                    reasons
                        .into_iter()
                        .map(|reason| format!("{}: {reason}", use_case.as_str())),
                ),
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }
}
