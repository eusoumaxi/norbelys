//! The template engine: one `minijinja` environment, bounded in work and in output, used for
//! every subject, body and preheader whether it is checked at creation or rendered for sending.
//!
//! # Why one environment
//!
//! A template that renders when a message is created must render the same when it is sent, so
//! both run here with the same settings: the creation check is the render itself, and its
//! errors become `422` problems pointing at the part (`/subject`, `/html`, `/text`) instead of
//! a message that fails later in the sender.
//!
//! # Bounds
//!
//! - **Fuel.** Every instruction the engine runs costs fuel, and one part may spend
//!   [`FUEL`]: a loop that would run forever, or a million times, stops with an error.
//! - **Bytes.** The output is written through a counter that refuses to grow a part beyond its
//!   limit ([`Part::limit`]), so a template that expands into megabytes stops at its first write
//!   past the limit. (The engine itself refuses a single repeated string above 100 MB and a
//!   `range` above 100,000 items, which bounds what one expression can allocate before it is
//!   written.)
//! - **Recursion.** The engine's nesting limit is lowered to [`RECURSION`].
//!
//! # Undefined values
//!
//! Printing a value that does not exist is an error (`SemiStrict`), not an empty string: a cold
//! email that says "Hi ," because a person has no first name is worse than a message refused at
//! creation. Testing a value for truth is allowed, so `{% if person.given_name %}` and
//! `{{ person.given_name | default("there") }}` are how a template handles a missing value.
//! The context never holds `null` (the namespaces drop absent values), because the engine would
//! print `none` for it.
//!
//! # Escaping
//!
//! HTML parts escape every printed value (a company named `<script>` stays text); the subject,
//! the text body and the preheader are plain text and escape nothing. The part's template name
//! (`body.html`, `body.txt`, …) selects the escaping, as the engine's default rule does by file
//! extension.

use std::io;
use std::sync::LazyLock;

use minijinja::{Environment, ErrorKind, UndefinedBehavior};

/// The fuel one part may spend: far above what a template of variables and a few loops needs.
pub const FUEL: u64 = 100_000;
/// How deeply blocks and expressions may nest.
pub const RECURSION: usize = 64;
/// The largest rendered subject, in bytes.
pub const SUBJECT_MAX: usize = 1_000;
/// The largest rendered body (HTML or text) or preheader, in bytes.
pub const BODY_MAX: usize = 1 << 20;

/// The environment every render uses; built once.
static ENVIRONMENT: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut environment = Environment::new();
    environment.set_undefined_behavior(UndefinedBehavior::SemiStrict);
    environment.set_fuel(Some(FUEL));
    environment.set_recursion_limit(RECURSION);
    environment
});

/// A part of a message that is a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Part {
    /// The subject: one line of plain text.
    Subject,
    /// The HTML body: printed values are HTML-escaped.
    Html,
    /// The plain-text body.
    Text,
    /// The hidden preview text at the top of an HTML body (campaign variants only).
    Preheader,
}

impl Part {
    /// The template's name, which selects its escaping.
    fn name(self) -> &'static str {
        match self {
            Self::Subject => "subject.txt",
            Self::Html => "body.html",
            Self::Text => "body.txt",
            Self::Preheader => "preheader.txt",
        }
    }

    /// The largest rendered output, in bytes.
    #[must_use]
    pub fn limit(self) -> usize {
        match self {
            Self::Subject => SUBJECT_MAX,
            Self::Html | Self::Text | Self::Preheader => BODY_MAX,
        }
    }

    /// The JSON pointer of the part in a request that carries it.
    #[must_use]
    pub fn pointer(self) -> &'static str {
        match self {
            Self::Subject => "/subject",
            Self::Html => "/html",
            Self::Text => "/text",
            Self::Preheader => "/preheader",
        }
    }
}

/// Why a template did not render.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{part:?}: {detail}")]
pub struct TemplateError {
    /// The part at fault.
    pub part: Part,
    /// What the engine said, with the line when it knows it; safe to show to the author.
    pub detail: String,
}

/// Renders `source` as `part` with `context` (an object of namespaces) under the bounds of the
/// module. A subject comes back as one trimmed line: line breaks and other control characters
/// become spaces, so no subject can carry a header of its own.
///
/// # Errors
///
/// A syntax error, an undefined value printed, a filter misused, fuel spent, nesting too deep
/// or an output beyond the part's limit.
pub fn render(
    part: Part,
    source: &str,
    context: &serde_json::Value,
) -> Result<String, TemplateError> {
    let fail = |error: &minijinja::Error| TemplateError {
        part,
        detail: describe(part, error),
    };
    let template = ENVIRONMENT
        .template_from_named_str(part.name(), source)
        .map_err(|error| fail(&error))?;
    let mut output = Capped {
        bytes: Vec::new(),
        limit: part.limit(),
    };
    template
        .render_captured_to(context, &mut output)
        .map_err(|error| fail(&error))?;
    let rendered = String::from_utf8(output.bytes).map_err(|_| TemplateError {
        part,
        detail: "the template produced text that is not UTF-8".to_owned(),
    })?;
    Ok(match part {
        Part::Subject => one_line(&rendered),
        Part::Html | Part::Text | Part::Preheader => rendered,
    })
}

/// Checks that `source` is a valid template for `part` without rendering it: the syntax alone,
/// which is what can be known before the values exist. A campaign variant is checked this way
/// when it is saved, and rendered for each person when their message is created.
///
/// # Errors
///
/// A syntax error, with its line.
pub fn check_syntax(part: Part, source: &str) -> Result<(), TemplateError> {
    ENVIRONMENT
        .template_from_named_str(part.name(), source)
        .map(|_| ())
        .map_err(|error| TemplateError {
            part,
            detail: describe(part, &error),
        })
}

/// The engine's error in words an author can act on.
fn describe(part: Part, error: &minijinja::Error) -> String {
    let what = match error.kind() {
        ErrorKind::OutOfFuel => {
            "the template does too much work (a loop that is too long, or never ends)".to_owned()
        }
        ErrorKind::WriteFailure => {
            format!("the rendered text is longer than {} bytes", part.limit())
        }
        ErrorKind::UndefinedError => format!(
            "{}; give a value a default (`| default(\"…\")`) or test it with `{{% if … %}}`",
            error.detail().unwrap_or("a value is undefined")
        ),
        kind => match error.detail() {
            Some(detail) => format!("{kind}: {detail}"),
            None => kind.to_string(),
        },
    };
    match error.line() {
        Some(line) => format!("line {line}: {what}"),
        None => what,
    }
}

/// One line: control characters (line breaks among them) become spaces, runs of spaces one.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_control() || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A writer that refuses to hold more than `limit` bytes.
struct Capped {
    bytes: Vec<u8>,
    limit: usize,
}

impl io::Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.limit {
            return Err(io::Error::other("the output limit is reached"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use strum::IntoEnumIterator as _;

    use super::*;

    fn context() -> serde_json::Value {
        json!({
            "person": { "email": "ada@example.com", "given_name": "Ada", "fields": { "plan": "pro" } },
            "sender": { "email": "max@norbelys.example", "name": "Max" },
            "variables": { "company": "<Analytical & Co>" },
        })
    }

    /// Values print in every part; HTML parts escape them and plain parts do not, so a value can
    /// never inject markup into a body, while a subject shows the company as written.
    #[test]
    fn values_print_escaped_only_in_html() {
        let source = "Hi {{ person.given_name }} from {{ variables.company }}";
        for part in Part::iter() {
            let rendered = render(part, source, &context()).unwrap();
            let expected = match part {
                Part::Html => "Hi Ada from &lt;Analytical &amp; Co&gt;",
                Part::Subject | Part::Text | Part::Preheader => "Hi Ada from <Analytical & Co>",
            };
            assert_eq!(rendered, expected, "{part:?}");
        }
    }

    /// A value that does not exist fails when printed, naming the line; a default or a truth
    /// test is how a template handles it, and both work.
    #[test]
    fn missing_values_fail_unless_handled() {
        let refused = render(Part::Text, "Hi\n{{ person.family_name }}", &context()).unwrap_err();
        assert_eq!(refused.part, Part::Text);
        assert!(refused.detail.starts_with("line 2:"), "{}", refused.detail);
        let handled =
            "{{ person.family_name | default(\"friend\") }}{% if person.fields.size %}!{% endif %}";
        assert_eq!(render(Part::Text, handled, &context()).unwrap(), "friend");
        assert!(render(Part::Text, "{{ nothing.at.all }}", &context()).is_err());
    }

    /// A template that never ends, or runs a loop far too long, stops on its fuel instead of
    /// holding the sender.
    #[test]
    fn work_is_bounded_by_fuel() {
        let refused = render(
            Part::Text,
            "{% for a in range(1000) %}{% for b in range(1000) %}.{% endfor %}{% endfor %}",
            &context(),
        )
        .unwrap_err();
        assert!(
            refused.detail.contains("too much work"),
            "{}",
            refused.detail
        );
    }

    /// Output is bounded per part: a subject beyond 1,000 bytes and a body beyond 1 MiB are
    /// refused, while the same body just under the limit renders.
    #[test]
    fn output_is_bounded_per_part() {
        let long_subject = render(Part::Subject, "{{ 'x' * 1001 }}", &context()).unwrap_err();
        assert!(long_subject.detail.contains("longer than 1000 bytes"));
        let huge = render(Part::Html, "{{ 'x' * 1048577 }}", &context()).unwrap_err();
        assert!(huge.detail.contains("longer than 1048576 bytes"));
        assert_eq!(
            render(Part::Html, "{{ 'x' * 1048576 }}", &context())
                .unwrap()
                .len(),
            BODY_MAX
        );
    }

    /// A subject is one line: a line break written in it, or printed into it, cannot start a
    /// header of its own.
    #[test]
    fn a_subject_is_one_line() {
        let context = json!({ "variables": { "x": "a\r\nBcc: victim@example.com" } });
        assert_eq!(
            render(Part::Subject, "  Hello\n{{ variables.x }}\t ", &context).unwrap(),
            "Hello a Bcc: victim@example.com"
        );
    }

    /// A syntax error is reported against its part with its line, as a creation check answers
    /// it to the author.
    #[test]
    fn syntax_errors_name_part_and_line() {
        let refused = render(Part::Html, "<p>\n{% if %}</p>", &context()).unwrap_err();
        assert_eq!(refused.part, Part::Html);
        assert!(refused.detail.starts_with("line 2:"), "{}", refused.detail);
    }
}
