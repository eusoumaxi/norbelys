//! The prompts of the AI use cases: versioned files, embedded at build time.
//!
//! A prompt is the file `crates/ai/prompts/<use case>/<name>-v<version>.txt`: a front matter of
//! `id`, `version` and `use_case` between two `---` lines, then the system prompt itself. Every
//! call records the id of the prompt it used (`classification/reply-v1`), so the calls a
//! prompt served and the evaluation that measured it always refer to the same text. A prompt is
//! therefore a revision: once released it is never edited; a change is a new file with the next
//! version.
//!
//! # How a new version ships
//!
//! 1. Its file is added and declared in [`CANARIES`] beside the current prompt of its use case.
//!    The evaluation then measures the canary, and the change merges only when it passes.
//! 2. Deployed, the canary serves a share of the calls for a day, guarded by its review rate
//!    against the current prompt's (`domain::ai::canary_phase`): a rise beyond the deployment's
//!    margin rolls it back, a day within it promotes it to every call. Every call row records
//!    which prompt served it and whether it served as a canary, so both outcomes are read from
//!    the rows and survive restarts.
//! 3. Once promoted, a later change makes it the current prompt in [`FILES`] and empties
//!    [`CANARIES`]; the old file stays, since past calls name it.
//!
//! Only classification has canaries: the review rate that guards them exists only for verdicts.
//! A change to another use case's prompt ships behind its evaluation alone.

use std::sync::LazyLock;

use crate::domain::ai::UseCase;

/// One released prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// `<use case>/<name>-v<version>`, as `ai_calls.prompt_id` records it.
    pub id: String,
    /// The version, also the file name's suffix.
    pub version: u32,
    /// The use case it serves, also its folder.
    pub use_case: UseCase,
    /// The system prompt.
    pub text: &'static str,
}

/// The prompt each use case uses now: (use case, file name without `.txt`, contents).
const FILES: [(UseCase, &str, &str); 3] = [
    (
        UseCase::Classification,
        "reply-v1",
        include_str!("../../../ai/prompts/classification/reply-v1.txt"),
    ),
    (
        UseCase::Snippets,
        "snippets-v1",
        include_str!("../../../ai/prompts/snippets/snippets-v1.txt"),
    ),
    (
        UseCase::Hints,
        "hygiene-v1",
        include_str!("../../../ai/prompts/hints/hygiene-v1.txt"),
    ),
];

/// The newer version each use case is moving to, if any, served as a canary beside its current
/// prompt (see the module): (use case, file name without `.txt`, contents), at most one per use
/// case, and only for classification.
const CANARIES: [(UseCase, &str, &str); 0] = [];

/// The parsed prompts, in the order of [`FILES`].
static PROMPTS: LazyLock<Vec<Prompt>> = LazyLock::new(|| parsed(&FILES));

/// The parsed canaries, in the order of [`CANARIES`].
static CANARY_PROMPTS: LazyLock<Vec<Prompt>> = LazyLock::new(|| parsed(&CANARIES));

/// Every file of `files`, parsed.
fn parsed(files: &[(UseCase, &str, &'static str)]) -> Vec<Prompt> {
    files
        .iter()
        .map(|&(use_case, name, source)| {
            parse(use_case, name, source).unwrap_or_else(|error| {
                unreachable!("the prompt {name} is checked by the tests: {error}")
            })
        })
        .collect()
}

/// The prompt `use_case` uses now.
#[must_use]
pub fn current(use_case: UseCase) -> &'static Prompt {
    PROMPTS
        .iter()
        .find(|prompt| prompt.use_case == use_case)
        .unwrap_or_else(|| unreachable!("every use case has a prompt, checked by the tests"))
}

/// The canary of `use_case`: the newer version it is moving to, when one is declared.
#[must_use]
pub fn canary(use_case: UseCase) -> Option<&'static Prompt> {
    CANARY_PROMPTS
        .iter()
        .find(|prompt| prompt.use_case == use_case)
}

/// Reads one prompt file: its front matter must name the file (`id`), the version its name ends
/// with, and the use case of its folder, and the prompt after it must not be empty.
fn parse(use_case: UseCase, name: &str, source: &'static str) -> Result<Prompt, &'static str> {
    let rest = source
        .strip_prefix("---\n")
        .ok_or("the file starts with a `---` line")?;
    let (front, text) = rest
        .split_once("\n---\n")
        .ok_or("the front matter ends with a `---` line")?;
    let mut id = None;
    let mut version = None;
    let mut named = None;
    for line in front.lines() {
        let (key, value) = line
            .split_once(':')
            .ok_or("each front matter line is `key: value`")?;
        let value = value.trim();
        match key.trim() {
            "id" => id = Some(value),
            "version" => version = value.parse::<u32>().ok(),
            "use_case" => named = value.parse::<UseCase>().ok(),
            _ => return Err("the front matter holds `id`, `version` and `use_case` only"),
        }
    }
    let version = version.ok_or("`version` is a number")?;
    if id != Some(name) || !name.ends_with(&format!("-v{version}")) {
        return Err("`id` is the file's name, which ends with `-v<version>`");
    }
    if named != Some(use_case) {
        return Err("`use_case` is the prompt's folder");
    }
    let text = text.trim();
    if text.is_empty() {
        return Err("the prompt follows the front matter");
    }
    Ok(Prompt {
        id: format!("{}/{name}", use_case.as_str()),
        version,
        use_case,
        text,
    })
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::{CANARIES, FILES, canary, current, parse};
    use crate::domain::ai::UseCase;

    /// Every use case has exactly one current prompt, and each prompt file reads: its id is its
    /// file name, which carries its version, and its folder is its use case. A new use case fails
    /// here until it has a prompt.
    #[test]
    fn every_use_case_has_a_released_prompt() {
        for use_case in UseCase::iter() {
            let prompt = current(use_case);
            assert_eq!(prompt.use_case, use_case);
            assert!(
                prompt.id.starts_with(&format!("{}/", use_case.as_str())),
                "{}",
                prompt.id
            );
            assert!(prompt.id.ends_with(&format!("-v{}", prompt.version)));
            assert!(!prompt.text.starts_with("---"));
            assert_eq!(
                FILES
                    .iter()
                    .filter(|(file_use, ..)| *file_use == use_case)
                    .count(),
                1
            );
        }
        assert_eq!(
            current(UseCase::Classification).id,
            "classification/reply-v1"
        );
    }

    /// A declared canary reads like any prompt, belongs to classification (the only use case
    /// whose verdicts give the review rate that guards it), is the only canary of its use case,
    /// and is a newer version than the current prompt, so its guard always compares a change with
    /// what it replaces. No other use case has one.
    #[test]
    fn canaries_are_newer_versions_of_a_classification_prompt() {
        for (use_case, ..) in CANARIES {
            assert_eq!(use_case, UseCase::Classification);
            let canary = canary(use_case).unwrap();
            assert!(canary.version > current(use_case).version, "{}", canary.id);
            assert_eq!(
                CANARIES
                    .iter()
                    .filter(|(declared, ..)| *declared == use_case)
                    .count(),
                1
            );
        }
        for use_case in UseCase::iter().filter(|&use_case| use_case != UseCase::Classification) {
            assert!(canary(use_case).is_none(), "{}", use_case.as_str());
        }
    }

    /// A file whose front matter does not match its name, version or folder is refused, so a
    /// copied prompt cannot silently keep its old id.
    #[test]
    fn a_mismatched_front_matter_is_refused() {
        let good = "---\nid: reply-v2\nversion: 2\nuse_case: classification\n---\nClassify.\n";
        assert!(parse(UseCase::Classification, "reply-v2", good).is_ok());
        for (name, source) in [
            ("reply-v3", good),
            (
                "reply-v2",
                "---\nid: reply-v2\nversion: 3\nuse_case: classification\n---\nX\n",
            ),
            (
                "reply-v2",
                "---\nid: reply-v2\nversion: 2\nuse_case: snippets\n---\nX\n",
            ),
            (
                "reply-v2",
                "---\nid: reply-v2\nversion: 2\nuse_case: classification\n---\n\n",
            ),
            ("reply-v2", "id: reply-v2\n"),
            (
                "reply-v2",
                "---\nid: reply-v2\nversion: 2\nuse_case: classification\nmodel: x\n---\nX\n",
            ),
        ] {
            assert!(
                parse(UseCase::Classification, name, source).is_err(),
                "{name}: {source}"
            );
        }
    }
}
