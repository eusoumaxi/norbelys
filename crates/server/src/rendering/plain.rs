//! The plain-text alternative derived from an authored HTML body.
//!
//! Authors write one body. The alternative preserves readable paragraphs and line breaks for
//! mail clients that do not display HTML. It is derived before tracking rewrites, so a reader of
//! the plain part sees the author's destination rather than a tracking link. Script and style
//! contents are never copied into the alternative.

use std::cell::RefCell;
use std::rc::Rc;

use lol_html::html_content::TextType;
use lol_html::{RewriteStrSettings, doc_text, element, end_tag, rewrite_str};

/// Derives the plain-text alternative from rendered HTML.
///
/// # Errors
///
/// The HTML rewriter cannot process the body within its memory bound.
pub fn from_html(html: &str) -> Result<String, lol_html::errors::RewritingError> {
    let output = Rc::new(RefCell::new(String::new()));
    let blocks = Rc::clone(&output);
    let breaks = Rc::clone(&output);
    let links = Rc::clone(&output);
    let words = Rc::clone(&output);
    rewrite_str(
        html,
        RewriteStrSettings::new()
            .with_strict(false)
            .with_enable_esi_tags(false)
            .append_element_content_handler(element!(
                "p,div,section,article,header,footer,h1,h2,h3,h4,h5,h6,ul,ol,li,blockquote,pre,tr",
                move |node| {
                    blocks.borrow_mut().push('\n');
                    let end = Rc::clone(&blocks);
                    node.on_end_tag(end_tag!(move |_| {
                        end.borrow_mut().push('\n');
                        Ok(())
                    }))
                }
            ))
            .append_element_content_handler(element!("br,hr", move |_| {
                breaks.borrow_mut().push('\n');
                Ok(())
            }))
            .append_element_content_handler(element!("a[href]", move |node| {
                if let Some(destination) = node.get_attribute("href") {
                    let end = Rc::clone(&links);
                    node.on_end_tag(end_tag!(move |_| {
                        end.borrow_mut().push_str(&format!(" ({destination})"));
                        Ok(())
                    }))?;
                }
                Ok(())
            }))
            .append_document_content_handler(doc_text!(move |chunk| {
                if chunk.text_type() == TextType::Data {
                    words.borrow_mut().push_str(chunk.as_str());
                }
                Ok(())
            })),
    )?;
    let output = output.borrow();
    let decoded = decode_entities(&output);
    let mut normalized = String::new();
    let mut pending_breaks = 0_usize;
    for segment in decoded.split_inclusive('\n') {
        let line = segment.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            if !normalized.is_empty() {
                normalized.push_str(match pending_breaks {
                    0 => " ",
                    1 => "\n",
                    _ => "\n\n",
                });
            }
            normalized.push_str(&line);
            pending_breaks = 0;
        }
        if segment.ends_with('\n') {
            pending_breaks = pending_breaks.saturating_add(1);
        }
    }
    Ok(normalized)
}

/// Decodes the entities commonly emitted by HTML templates, including numeric entities.
fn decode_entities(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('&') {
        result.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|end| *end <= 12) else {
            result.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|digits| digits.parse().ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => result.push(c),
            None => result.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An authored HTML body becomes readable plain text with decoded values and useful breaks.
    #[test]
    fn derives_plain_alternative() {
        let html =
            "<p>Hello Ada &amp; Max,</p><p>A short <b>introduction</b>.<br>See &#x2605;.</p>";
        assert_eq!(
            from_html(html).unwrap(),
            "Hello Ada & Max,\n\nA short introduction.\nSee ★."
        );
    }

    /// Non-message source code in an HTML body does not leak into the plain alternative.
    #[test]
    fn omits_script_and_style() {
        assert_eq!(
            from_html("<style>p{color:red}</style><p>Visible</p><script>alert(1)</script>")
                .unwrap(),
            "Visible"
        );
    }

    /// The plain part keeps the author's destination instead of losing it with the HTML anchor.
    #[test]
    fn links_keep_their_destination() {
        assert_eq!(
            from_html("<p>Read <a href=\"https://example.com/?a=1&amp;b=2\">the guide</a>.</p>")
                .unwrap(),
            "Read the guide (https://example.com/?a=1&b=2)."
        );
    }
}
