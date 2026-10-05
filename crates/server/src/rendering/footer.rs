//! What Norbelys adds around an author's rendered body: the preheader at the top of an HTML
//! body, and at the bottom the sender identity's signature and, on campaign mail, the
//! unsubscribe link.
//!
//! - **Preheader.** The short text a mail client shows after the subject in its list. It is
//!   plain text, escaped, in an element no client displays, as the first thing in the body.
//! - **Signature.** The identity's signature ends both parts, whatever kind of mail it is: an
//!   identity's signature is part of how it writes. A person writes it once, as HTML or as plain
//!   text, and the other part's is derived from it when the message is rendered ([`Signature::of`],
//!   the one place that decides it), the way a body's plain-text alternative is derived from its
//!   HTML: only `signature_html` set, the text part ends with its plain text
//!   ([`super::plain::from_html`], trimmed); only `signature_text` set, the HTML part ends with the
//!   text escaped, its line breaks written as `<br>` and nothing else changed; both set, each part
//!   ends with its own as written, so a developer keeps full control of each; neither (or only
//!   blank ones), no signature.
//! - **Unsubscribe.** Campaign mail carries a visible unsubscribe link besides its
//!   `List-Unsubscribe` header. An author who placed `{{ unsubscribe_url }}` in the body chose
//!   where it goes; otherwise one short line is added at the end. The link's token is unique and
//!   survives HTML escaping unchanged (it is base64url), so finding it tells whether the author
//!   placed it.
//!
//! An HTML body with a `<body>` element gets the additions inside it (after its start tag and
//! before its end tag); a fragment without one (`<p>…</p>`, what editors and the API usually
//! produce) gets them before and after it.

use std::cell::Cell;

use lol_html::html_content::ContentType;
use lol_html::{RewriteStrSettings, element, rewrite_str};

/// The unsubscribe link of a campaign message.
#[derive(Debug, Clone, Copy)]
pub struct Unsubscribe<'a> {
    /// The absolute URL.
    pub url: &'a str,
    /// The token inside it, which identifies the link in a rendered body.
    pub token: &'a str,
}

/// What is added around one message's bodies.
#[derive(Debug, Clone, Copy, Default)]
pub struct Additions<'a> {
    /// The rendered preheader (campaign variants).
    pub preheader: Option<&'a str>,
    /// The signature that ends the HTML body: the identity's [`Signature`]'s `html`.
    pub signature_html: Option<&'a str>,
    /// The signature that ends the text body: the identity's [`Signature`]'s `text`.
    pub signature_text: Option<&'a str>,
    /// The unsubscribe link (campaign mail).
    pub unsubscribe: Option<Unsubscribe<'a>>,
}

/// An identity's signature as each part of its mail ends with it (see the module).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signature {
    /// What ends the HTML part.
    pub html: Option<String>,
    /// What ends the text part.
    pub text: Option<String>,
}

impl Signature {
    /// The signature of an identity that wrote `html`, `text`, both or neither (a blank one counts
    /// as not written): each part ends with its own form as written, and a part whose form was not
    /// written derives it from the other (see the module). A derived text that comes out empty
    /// (an HTML signature of images alone) leaves the text part without a signature.
    ///
    /// # Errors
    ///
    /// The HTML signature could not be read as text (it exhausted the rewriter's memory limit), as
    /// a body's plain-text alternative can fail.
    pub fn of(
        html: Option<&str>,
        text: Option<&str>,
    ) -> Result<Self, lol_html::errors::RewritingError> {
        let html = html.filter(|html| !html.trim().is_empty());
        let text = text.filter(|text| !text.trim().is_empty());
        Ok(match (html, text) {
            (Some(html), Some(text)) => Self {
                html: Some(html.to_owned()),
                text: Some(text.to_owned()),
            },
            (Some(html), None) => {
                let derived = super::plain::from_html(html)?;
                let derived = derived.trim();
                Self {
                    html: Some(html.to_owned()),
                    text: (!derived.is_empty()).then(|| derived.to_owned()),
                }
            }
            (None, Some(text)) => Self {
                html: Some(
                    escape(text)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                        .replace('\n', "<br>"),
                ),
                text: Some(text.to_owned()),
            },
            (None, None) => Self::default(),
        })
    }
}

/// The HTML body with its additions.
///
/// # Errors
///
/// The HTML could not be rewritten (it exhausted the rewriter's memory limit).
pub fn html(
    body: &str,
    additions: &Additions<'_>,
) -> Result<String, lol_html::errors::RewritingError> {
    let start = additions
        .preheader
        .filter(|preheader| !preheader.trim().is_empty())
        .map(|preheader| {
            format!(
                "<div style=\"display:none;max-height:0;overflow:hidden;mso-hide:all\">{}</div>",
                escape(preheader.trim())
            )
        })
        .unwrap_or_default();
    let mut end = String::new();
    if let Some(signature) = additions.signature_html.filter(|s| !s.trim().is_empty()) {
        end.push_str("<div style=\"margin-top:16px\">");
        end.push_str(signature);
        end.push_str("</div>");
    }
    if let Some(unsubscribe) = additions.unsubscribe
        && !body.contains(unsubscribe.token)
    {
        end.push_str(&format!(
            "<p style=\"margin-top:24px;font-size:12px;color:#888888\"><a href=\"{}\" style=\"color:#888888\">Unsubscribe</a></p>",
            escape(unsubscribe.url)
        ));
    }
    around_body(body, &start, &end)
}

/// The text body with its additions.
#[must_use]
pub fn text(body: &str, additions: &Additions<'_>) -> String {
    let mut text = body.to_owned();
    if let Some(signature) = additions.signature_text.filter(|s| !s.trim().is_empty()) {
        text.push_str("\n\n");
        text.push_str(signature);
    }
    if let Some(unsubscribe) = additions.unsubscribe
        && !body.contains(unsubscribe.token)
    {
        text.push_str("\n\nUnsubscribe: ");
        text.push_str(unsubscribe.url);
        text.push('\n');
    }
    text
}

/// `html` with `start` placed right after its `<body>` start tag and `end` right before its end
/// tag; before and after the whole document when it has no `<body>`. Empty parts add nothing.
///
/// # Errors
///
/// The HTML could not be rewritten.
pub fn around_body(
    html: &str,
    start: &str,
    end: &str,
) -> Result<String, lol_html::errors::RewritingError> {
    if start.is_empty() && end.is_empty() {
        return Ok(html.to_owned());
    }
    let found = Cell::new(false);
    let rewritten = rewrite_str(
        html,
        RewriteStrSettings::new()
            .with_strict(false)
            .with_enable_esi_tags(false)
            .append_element_content_handler(element!("body", |body| {
                if !found.replace(true) {
                    body.prepend(start, ContentType::Html);
                    body.append(end, ContentType::Html);
                }
                Ok(())
            })),
    )?;
    Ok(if found.get() {
        rewritten
    } else {
        format!("{start}{rewritten}{end}")
    })
}

/// `text` with the five characters HTML gives meaning to escaped.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNSUBSCRIBE: Unsubscribe<'static> = Unsubscribe {
        url: "https://t.example/u/TOKEN123",
        token: "TOKEN123",
    };

    /// A fragment gets the preheader before it and the signature and unsubscribe line after it;
    /// a document gets them inside its `<body>`, so nothing lands after `</html>`.
    #[test]
    fn additions_go_inside_the_body_or_around_a_fragment() {
        let additions = Additions {
            preheader: Some("Quick <question>"),
            signature_html: Some("<b>Max</b>"),
            signature_text: None,
            unsubscribe: Some(UNSUBSCRIBE),
        };
        let fragment = html("<p>Hi</p>", &additions).unwrap();
        assert!(
            fragment.starts_with("<div style=\"display:none;"),
            "{fragment}"
        );
        assert!(fragment.contains("Quick &lt;question&gt;</div><p>Hi</p><div"));
        assert!(fragment.contains("<b>Max</b>"));
        assert!(fragment.ends_with("Unsubscribe</a></p>"), "{fragment}");
        let document = html(
            "<html><body class=\"x\"><p>Hi</p></body></html>",
            &additions,
        )
        .unwrap();
        assert!(document.starts_with("<html><body class=\"x\"><div style=\"display:none;"));
        assert!(
            document.ends_with("Unsubscribe</a></p></body></html>"),
            "{document}"
        );
    }

    /// An author who placed the unsubscribe link gets no second one; the text body follows the
    /// same rule, and gets the text signature.
    #[test]
    fn a_placed_unsubscribe_link_is_not_repeated() {
        let additions = Additions {
            signature_text: Some("Max\nNorbelys"),
            unsubscribe: Some(UNSUBSCRIBE),
            ..Additions::default()
        };
        let placed = "<a href=\"https:&#x2f;&#x2f;t.example&#x2f;u&#x2f;TOKEN123\">stop</a>";
        assert_eq!(html(placed, &additions).unwrap(), placed);
        assert_eq!(
            text("Hi", &additions),
            "Hi\n\nMax\nNorbelys\n\nUnsubscribe: https://t.example/u/TOKEN123\n"
        );
        assert_eq!(
            text("Hi, stop here: https://t.example/u/TOKEN123", &additions),
            "Hi, stop here: https://t.example/u/TOKEN123\n\nMax\nNorbelys"
        );
    }

    /// Without additions a body is left exactly as rendered.
    #[test]
    fn nothing_added_leaves_the_body_alone() {
        let body = "<html><body><p>Hi</p></body></html>";
        assert_eq!(html(body, &Additions::default()).unwrap(), body);
        assert_eq!(text("Hi", &Additions::default()), "Hi");
    }

    /// A signature written only as HTML ends the text part too, as its plain text (entities
    /// decoded, paragraphs kept, trimmed), so a reader of either part sees who wrote.
    #[test]
    fn an_html_signature_alone_gives_the_text_part_its_plain_text() {
        let html = "<p><b>Max</b> &amp; Ada</p><p>Acme</p>";
        assert_eq!(
            Signature::of(Some(html), None).unwrap(),
            Signature {
                html: Some(html.to_owned()),
                text: Some("Max & Ada\n\nAcme".to_owned()),
            }
        );
    }

    /// A signature written only as text (an HTML one left blank) ends the HTML part too, escaped
    /// so it reads as written, each line break a `<br>`, nothing else changed.
    #[test]
    fn a_text_signature_alone_gives_the_html_part_its_escaped_lines() {
        let text = "-- \nMax & Ada\r\n<Acme>";
        assert_eq!(
            Signature::of(Some("  "), Some(text)).unwrap(),
            Signature {
                html: Some("-- <br>Max &amp; Ada<br>&lt;Acme&gt;".to_owned()),
                text: Some(text.to_owned()),
            }
        );
    }

    /// An identity that wrote both forms gets each in its own part exactly as written: a
    /// developer keeps full control of each part.
    #[test]
    fn both_signatures_are_used_as_written() {
        assert_eq!(
            Signature::of(Some("<p>Max</p>"), Some("Max, Acme")).unwrap(),
            Signature {
                html: Some("<p>Max</p>".to_owned()),
                text: Some("Max, Acme".to_owned()),
            }
        );
    }
}
