//! `message.generate`: a step message whose content an AI personalises, created once its
//! snippets are written.
//!
//! A step with a personalisation prompt does not get its message from the creation pass
//! directly. The pass assigns the sender and the variant, records them, and enqueues this job
//! (unique per enrollment and step revision); the enrollment keeps `message_id` null meanwhile.
//! The job:
//!
//! 1. reads the step's prompt, the snippet names the variant's templates use
//!    (`{{ variables.opener }}` uses `opener`), and the person's fields, and checks the
//!    enrollment still waits for this step's message (else it is done: a stale job creates
//!    nothing);
//! 2. asks the AI for the snippets (`ai::snippets::generate`), outside any transaction, with
//!    the message's due time as its deadline (at least two minutes from now). Anything short of
//!    valid snippets in time (snippets switched off, no budget, a refusal, the deadline) gives
//!    the template's defaults instead: a message is never held for AI, and a template handles a
//!    missing snippet with `| default("…")`;
//! 3. in its checkpoint, creates the message through the creation contract with the snippets
//!    as its `variables`, after checking that its sender is still in the pool. The message row
//!    is inserted whole, with its content's context, and never written afterwards.
//!
//! The workspace is told when the defaults were used: the message records why
//! (`messages.snippets_fallback`, shown by the API), and the same transaction records the
//! `message.snippets_fallback` event, so a webhook consumer hears of it exactly when the message
//! exists. When the workspace itself turned snippets off, the message records it but no event is
//! sent: the workspace chose it, and one event per message would only repeat that choice.
//!
//! The effect class is the AI calls': an external call that may be repeated, its spend settled
//! by the AI module's recovery hook when a run is interrupted.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::creator;
use crate::ai::snippets::{self, Fallback, Snippets, SnippetsRequest};
use crate::crypto::Keys;
use crate::db::Tx;
use crate::delivery::accept::StepMessage;
use crate::domain::ids::{Campaign, Enrollment, Id, Message, Step, WorkspaceId};
use crate::domain::time::Timestamp;
use crate::jobs::{Effect, Job, JobContext, JobError, Outcome, Queue, RecoveryHook};
use crate::webhooks::EventType;
use crate::webhooks::outbox::{self, Event};

/// The names a template reads from `variables`.
static NAMES: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\bvariables\.([a-z][a-z0-9_]{0,63})\b").ok());

/// The least time the AI is given when a message is already due.
const LEAST_TIME: Duration = Duration::from_secs(120);

/// `message.generate`: one enrollment's step message, its snippets written first (see the
/// module). The payload is the step message the creation pass decided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageGenerate {
    pub enrollment: Uuid,
    pub campaign: Uuid,
    pub step: Uuid,
    /// The position the enrollment must still be at.
    pub position: i32,
    /// The step revision the job was keyed on: a newer revision makes the job stale.
    pub step_revision: i32,
    pub variant: Uuid,
    pub variant_version: i32,
    /// The conversation's sender.
    pub identity: Uuid,
    /// When the message is due.
    pub send_at: Option<Timestamp>,
}

impl Job for MessageGenerate {
    const KIND: &'static str = "message.generate";
    const QUEUE: Queue = Queue::Ai;
    const EFFECT: Effect = Effect::ExternalRetryable;
    const RECOVERY_HOOK: Option<RecoveryHook> = Some(crate::ai::store::settle_abandoned);

    fn unique_key(&self) -> Option<String> {
        Some(format!("{}:{}", self.enrollment, self.step_revision))
    }

    async fn run(self, cx: &mut JobContext) -> Result<Outcome, JobError> {
        let workspace = cx.workspace();
        let mut tx = cx.db().begin_in(workspace).await?;
        let input = sqlx::query!(
            r#"SELECT r.personalisation_prompt, c.subject, c.preheader, c.html, c.text,
                      p.given_name, p.family_name, p.company, p.custom_fields,
                      (e.status = 'active' AND e.current_position = $6 AND e.message_id IS NULL) AS "ready!"
                 FROM enrollments e
                 JOIN people p ON p.workspace_id = e.workspace_id AND p.id = e.person_id
                 JOIN step_revisions r ON r.workspace_id = e.workspace_id AND r.step_id = $3 AND r.revision = $4
                 JOIN variant_revisions c ON c.workspace_id = e.workspace_id AND c.variant_id = $5 AND c.version = $7
                WHERE e.workspace_id = $1 AND e.id = $2"#,
            workspace.uuid(),
            self.enrollment,
            self.step,
            self.step_revision,
            self.variant,
            self.position,
            self.variant_version,
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(input) = input.filter(|input| input.ready) else {
            return Ok(Outcome::Done);
        };
        let names = names(&[
            Some(input.subject.as_str()),
            input.preheader.as_deref(),
            Some(input.html.as_str()),
            input.text.as_deref(),
        ]);
        let (snippets, fallback) = match input.personalisation_prompt.as_deref() {
            Some(instructions) if !names.is_empty() => {
                let mut fields = match input.custom_fields {
                    Value::Object(fields) => fields,
                    _ => Map::new(),
                };
                for (key, value) in [
                    ("given_name", input.given_name),
                    ("family_name", input.family_name),
                    ("company", input.company),
                ] {
                    if let Some(value) = value {
                        fields.insert(key.to_owned(), Value::String(value));
                    }
                }
                let least = crate::process::now().plus(LEAST_TIME);
                let deadline = self.send_at.map_or(least, |at| at.max(least));
                let request = SnippetsRequest {
                    instructions,
                    names: &names,
                    fields: &fields,
                    deadline: Some(deadline),
                };
                match snippets::generate(cx, &request).await? {
                    Snippets::Generated(written) => (
                        Some(
                            written
                                .into_iter()
                                .map(|(name, text)| (name, Value::String(text)))
                                .collect::<Map<String, Value>>(),
                        ),
                        None,
                    ),
                    Snippets::Defaults(fallback) => (None, Some(fallback)),
                }
            }
            _ => (None, None),
        };
        let keys = cx.env::<Keys>()?.clone();
        let message = StepMessage {
            snippets_fallback: fallback.map(Fallback::as_str),
            ..self.message()
        };
        let mut chunk = cx.begin().await?;
        let created = creator::create_generated(
            chunk.tx(),
            &keys,
            workspace,
            &message,
            snippets,
            super::recipients::validator(cx).enabled(),
        )
        .await?;
        if created && let Some(fallback) = fallback.filter(|fallback| *fallback != Fallback::Off) {
            tell(chunk.tx(), workspace, &self, fallback).await?;
        }
        cx.checkpoint(
            chunk,
            json!({ "created": created, "snippets_fallback": fallback.map(Fallback::as_str) }),
        )
        .await?;
        if created {
            crate::delivery::accept::wake(cx.db()).await;
        }
        Ok(Outcome::Done)
    }
}

/// Tells the workspace that the step message `job` just created in `tx` uses its template's
/// defaults because of `fallback`: one `message.snippets_fallback` event in the outbox, committed
/// with the message or not at all. The message is the one `job`'s enrollment now points at, which
/// the creation set in the same transaction.
///
/// # Errors
///
/// The database refused.
async fn tell(
    tx: &mut Tx,
    workspace: WorkspaceId,
    job: &MessageGenerate,
    fallback: Fallback,
) -> Result<(), sqlx::Error> {
    let message = sqlx::query_scalar!(
        "SELECT message_id FROM enrollments WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        job.enrollment,
    )
    .fetch_one(&mut **tx)
    .await?;
    let Some(message) = message else {
        return Ok(());
    };
    outbox::record(
        tx,
        workspace,
        Event {
            kind: EventType::MessageSnippetsFallback,
            subject_type: "message",
            subject_id: message,
            data: json!({
                "message_id": Id::<Message>::from_uuid(message),
                "enrollment_id": Id::<Enrollment>::from_uuid(job.enrollment),
                "campaign_id": Id::<Campaign>::from_uuid(job.campaign),
                "step_id": Id::<Step>::from_uuid(job.step),
                "reason": fallback.as_str(),
            }),
        },
    )
    .await?;
    Ok(())
}

/// The snippet names `templates` read from `variables` (`{{ variables.opener }}` reads `opener`),
/// sorted, each once. A step with a personalisation prompt is refused when one of its variants
/// reads more than the AI writes for one message, so the list is never cut here.
pub(crate) fn names(templates: &[Option<&str>]) -> Vec<String> {
    let Some(pattern) = NAMES.as_ref() else {
        return Vec::new();
    };
    let mut names: Vec<String> = templates
        .iter()
        .flatten()
        .flat_map(|template| pattern.captures_iter(template))
        .filter_map(|captures| captures.get(1))
        .map(|name| name.as_str().to_owned())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use strum::IntoEnumIterator as _;
    use uuid::Uuid;

    use super::{MessageGenerate, names};
    use crate::ai::snippets::Fallback;
    use crate::jobs::runner::Harness;
    use crate::jobs::{self, Queue, Registry};
    use crate::testing::{SenderSpec, TestDb, keys};

    /// A step asking for snippets that gets none (here: the deployment has no AI provider) has its
    /// message created with the template's defaults, the reason recorded on the message for the
    /// API to show, and the workspace told by one `message.snippets_fallback` event, written with
    /// the message, naming it, its enrollment, campaign and step, and the reason. Without the
    /// event and the field, the fallback was a log line nobody in the workspace sees.
    #[tokio::test]
    async fn a_fallback_is_recorded_on_the_message_and_told() {
        let test = TestDb::new().await;
        let workspace = test.workspace("acme").await.id;
        let sender = test
            .sender(workspace, &SenderSpec::relay("hello@acme.test"))
            .await;
        let campaign = test.campaign(workspace, &sender, None).await;
        let step: Uuid = sqlx::query_scalar(
            "UPDATE step_revisions SET personalisation_prompt = 'Open with their company.'
              WHERE workspace_id = $1
                AND step_id IN (SELECT id FROM steps WHERE workspace_id = $1 AND campaign_id = $2)
             RETURNING step_id",
        )
        .bind(workspace.uuid())
        .bind(campaign)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        let variant: Uuid = sqlx::query_scalar(
            "UPDATE variant_revisions SET html = '<p>{{ variables.opener | default(\"Hello\") }}</p>'
              WHERE workspace_id = $1
                AND variant_id IN (SELECT id FROM variants WHERE workspace_id = $1 AND step_id = $2)
             RETURNING variant_id",
        )
        .bind(workspace.uuid())
        .bind(step)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        let enrollment: Uuid = sqlx::query_scalar(
            "WITH p AS (INSERT INTO people (workspace_id, email) VALUES ($1, 'ada@example.com') RETURNING id)
             INSERT INTO enrollments (workspace_id, campaign_id, person_id) SELECT $1, $2, id FROM p RETURNING id",
        )
        .bind(workspace.uuid())
        .bind(campaign)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        let job = MessageGenerate {
            enrollment,
            campaign,
            step,
            position: 1,
            step_revision: 1,
            variant,
            variant_version: 1,
            identity: sender.identity.uuid(),
            send_at: None,
        };
        let mut tx = test.worker.begin_in(workspace).await.unwrap();
        jobs::enqueue(&mut tx, workspace, &job, None).await.unwrap();
        tx.commit().await.unwrap();
        let mut registry = Registry::default();
        registry.register::<MessageGenerate>().unwrap();
        let mut env = http::Extensions::new();
        env.insert(keys());
        let harness = Harness::new(
            test.worker.clone(),
            test.system.clone(),
            registry,
            env,
            "generate-test",
        );
        let ran = harness.run_once(Queue::Ai, 1).await;
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].1, "done");
        let progress: Option<Value> = sqlx::query_scalar(
            "SELECT progress FROM jobs WHERE workspace_id = $1 AND kind = 'message.generate'",
        )
        .bind(workspace.uuid())
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(
            progress,
            Some(json!({"created": true, "snippets_fallback": "unavailable"})),
            "the message was created, with the template's defaults"
        );
        let (message, fallback): (Uuid, Option<String>) = sqlx::query_as(
            "SELECT id, snippets_fallback FROM messages WHERE workspace_id = $1 AND enrollment_id = $2",
        )
        .bind(workspace.uuid())
        .bind(enrollment)
        .fetch_one(test.system.pool())
        .await
        .unwrap();
        assert_eq!(fallback.as_deref(), Some("unavailable"));
        let told: Vec<(Uuid, Value)> = sqlx::query_as(
            "SELECT subject_id, payload -> 'data' FROM outbox_events
              WHERE workspace_id = $1 AND type = 'message.snippets_fallback'",
        )
        .bind(workspace.uuid())
        .fetch_all(test.system.pool())
        .await
        .unwrap();
        assert_eq!(told.len(), 1);
        let (subject, data) = &told[0];
        assert_eq!(*subject, message);
        assert_eq!(
            *data,
            json!({
                "message_id": format!("msg_{}", message.simple()),
                "enrollment_id": format!("enr_{}", enrollment.simple()),
                "campaign_id": format!("cmp_{}", campaign.simple()),
                "connection_id": sender.connection,
                "step_id": format!("stp_{}", step.simple()),
                "reason": "unavailable",
            })
        );
    }

    /// Every reason a step message can fall back for is a value the message's column admits, and
    /// nothing else is: a reason the column refused would fail the creation of the message it
    /// describes. Only a campaign message carries one.
    #[tokio::test]
    async fn the_message_column_admits_every_fallback_reason() {
        let test = TestDb::new().await;
        let workspace = test.workspace("acme").await.id;
        let sender = test
            .sender(workspace, &SenderSpec::relay("hello@acme.test"))
            .await;
        let campaign = test.campaign(workspace, &sender, None).await;
        let step_message = test
            .campaign_message(workspace, campaign, &sender, "ada@example.com", 0)
            .await;
        let direct = test
            .direct_message(workspace, &sender, &["bob@example.com"], 0)
            .await;
        let set = |message: Uuid, reason: &'static str| {
            let test = &test;
            async move {
                sqlx::query(
                    "UPDATE messages SET snippets_fallback = $3 WHERE workspace_id = $1 AND id = $2",
                )
                .bind(workspace.uuid())
                .bind(message)
                .bind(reason)
                .execute(test.system.pool())
                .await
            }
        };
        for fallback in Fallback::iter() {
            let result = set(step_message.uuid(), fallback.as_str()).await;
            assert!(result.is_ok(), "{fallback:?}: {result:?}");
        }
        assert!(set(step_message.uuid(), "someday").await.is_err());
        assert!(set(direct.uuid(), "deadline").await.is_err());
    }

    /// The snippet names are the `variables` members the templates print or test, across every
    /// part, each once and in order: what the AI is asked to write, no more.
    #[test]
    fn snippet_names_are_what_the_templates_read() {
        assert_eq!(
            names(&[
                Some("Hi {{ person.given_name }}, {{ variables.opener | default(\"hello\") }}"),
                None,
                Some(
                    "<p>{{ variables.opener }}</p>{% if variables.ps %}{{ variables.ps }}{% endif %}"
                ),
                Some("{{variables.zeta}} and {{ sender.name }}"),
            ]),
            ["opener", "ps", "zeta"]
        );
        assert!(names(&[Some("no snippets here"), None]).is_empty());
    }
}
