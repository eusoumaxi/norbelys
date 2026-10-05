//! Open and click tracking in an HTML body: each link becomes a click link that redirects to
//! it, and an invisible pixel is added at the end, both on the message's tracking host.
//!
//! # Which links are rewritten
//!
//! Only `http` and `https` links that the recipient follows to somewhere else: a `mailto:`,
//! `tel:` or `sms:` link, an anchor (`#top`), a relative or a `javascript:` link stays as it is,
//! and so does a link to the tracking host itself (the unsubscribe link must reach the
//! unsubscribe page directly, never through a redirect a scanner could follow). An author keeps
//! one link untracked with the attribute `data-norbelys-no-track`. A link longer than the token
//! may carry ([`token::URL_MAX`]) stays as written. Each rewritten link's token carries its
//! position among the rewritten links (0-based, in document order) and its destination, so the
//! redirect needs no database.
//!
//! An `href` is read as written in the HTML, where `&` is `&amp;` (the template engine escapes
//! printed values, `/` as `&#x2f;` among them), so it is decoded before it goes into the token:
//! the redirect then leads exactly where the browser would have gone.
//!
//! # The pixel
//!
//! A 1×1 image at the end of the body (inside `<body>` when there is one). Mail clients that
//! load images then ask for it; many fetch images through a proxy or a scanner, which is why
//! the tracking role classifies who asked rather than counting every request as a person.

use std::cell::Cell;

use lol_html::{RewriteStrSettings, element, rewrite_str};

use super::footer;
use crate::crypto::Keys;
use crate::domain::ids::{Id, Message, WorkspaceId};
use crate::tracking::token::{self, Token};

/// What to track in one message's HTML body.
#[derive(Clone, Copy)]
pub struct Tracking<'a> {
    pub keys: &'a Keys,
    /// The tracking host's origin (`https://host`), where `/t/o/…` and `/t/c/…` live.
    pub origin: &'a str,
    pub workspace: WorkspaceId,
    pub message: Id<Message>,
    /// Add the open pixel.
    pub opens: bool,
    /// Rewrite the links.
    pub clicks: bool,
}

/// `html` with its links rewritten and its pixel added, as `tracking` asks.
///
/// # Errors
///
/// The HTML could not be rewritten (it exhausted the rewriter's memory limit).
pub fn rewrite(
    html: &str,
    tracking: &Tracking<'_>,
) -> Result<String, lol_html::errors::RewritingError> {
    let mut html = html.to_owned();
    if tracking.clicks {
        let next = Cell::new(0_u16);
        let own = tracking.origin.trim_end_matches('/').to_ascii_lowercase();
        html = rewrite_str(
            &html,
            RewriteStrSettings::new()
                .with_strict(false)
                .with_enable_esi_tags(false)
                .append_element_content_handler(element!("a[href]", |link| {
                    if link.has_attribute("data-norbelys-no-track") {
                        return Ok(());
                    }
                    let Some(href) = link.get_attribute("href") else {
                        return Ok(());
                    };
                    let url = decode_entities(href.trim());
                    let index = next.get();
                    if !trackable(&url, &own) || index == u16::MAX {
                        return Ok(());
                    }
                    let click = Token::Click {
                        workspace: tracking.workspace,
                        message: tracking.message,
                        link: index,
                        url,
                    };
                    link.set_attribute("href", &click.url(tracking.keys, tracking.origin))?;
                    next.set(index.saturating_add(1));
                    Ok(())
                })),
        )?;
    }
    if tracking.opens {
        let open = Token::Open {
            workspace: tracking.workspace,
            message: tracking.message,
        };
        let pixel = format!(
            "<img src=\"{}\" width=\"1\" height=\"1\" alt=\"\" style=\"display:block;border:0;width:1px;height:1px\">",
            open.url(tracking.keys, tracking.origin)
        );
        html = footer::around_body(&html, "", &pixel)?;
    }
    Ok(html)
}

/// Whether a link's decoded destination is rewritten: an absolute `http` or `https` URL short
/// enough for a token, not on our own tracking host (`own`, lowercase, no trailing slash).
fn trackable(url: &str, own: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let web = lower.starts_with("https://") || lower.starts_with("http://");
    let ours = lower == own
        || lower
            .strip_prefix(own)
            .is_some_and(|rest| rest.starts_with(['/', '?', '#']));
    web && !ours && url.len() <= token::URL_MAX && !url.chars().any(char::is_whitespace)
}

/// Decodes the character references an attribute value may hold: `&amp;`, `&lt;`, `&gt;`,
/// `&quot;`, `&apos;` and numeric ones (`&#38;`, `&#x2f;`). Anything else is left as written.
fn decode_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        let (before, from) = rest.split_at(at);
        decoded.push_str(before);
        let reference = from
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| from.get(1..end).map(|name| (name, end)));
        let character = reference.and_then(|(name, _)| match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .map(|hex| u32::from_str_radix(hex, 16))
                .or_else(|| name.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        });
        match (character, reference) {
            (Some(character), Some((_, end))) => {
                decoded.push(character);
                rest = from.get(end + 1..).unwrap_or_default();
            }
            _ => {
                decoded.push('&');
                rest = from.get(1..).unwrap_or_default();
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::testing::keys;

    fn tracking(keys: &Keys, opens: bool, clicks: bool) -> Tracking<'_> {
        Tracking {
            keys,
            origin: "https://t.example",
            workspace: WorkspaceId::trusted(Uuid::now_v7()),
            message: Id::new(),
            opens,
            clicks,
        }
    }

    /// The destinations of the rewritten links, in order, read back from their tokens.
    fn destinations(keys: &Keys, html: &str) -> Vec<(u16, String)> {
        html.split("https://t.example/t/c/")
            .skip(1)
            .map(|rest| {
                let token = rest.split('"').next().unwrap();
                match Token::decode(keys, token).unwrap() {
                    Token::Click { link, url, .. } => (link, url),
                    other => panic!("not a click: {other:?}"),
                }
            })
            .collect()
    }

    /// Web links become click links numbered in document order, each carrying its exact
    /// destination: the escaped `&amp;` and `&#x2f;` of a rendered `href` are decoded first, so
    /// the redirect leads where the browser would have gone.
    #[test]
    fn web_links_become_numbered_click_links() {
        let keys = keys();
        let html = "<a href=\"https://example.com/a?x=1&amp;y=2\">a</a> <a href=\"HTTP://Example.com&#x2f;b\">b</a>";
        let rewritten = rewrite(html, &tracking(&keys, false, true)).unwrap();
        assert_eq!(
            destinations(&keys, &rewritten),
            vec![
                (0, "https://example.com/a?x=1&y=2".to_owned()),
                (1, "HTTP://Example.com/b".to_owned())
            ]
        );
        assert!(rewritten.contains(">a</a> <a href="));
    }

    /// Links that must not be rewritten stay exactly as written: other schemes, anchors,
    /// relative links, links to the tracking host itself (the unsubscribe link), links the author
    /// marked, and links too long for a token.
    #[test]
    fn links_that_must_not_be_tracked_stay() {
        let keys = keys();
        let long = format!("https://example.com/{}", "x".repeat(token::URL_MAX));
        for href in [
            "mailto:ada@example.com".to_owned(),
            "tel:+15551234".to_owned(),
            "#top".to_owned(),
            "/relative/path".to_owned(),
            "javascript:alert(1)".to_owned(),
            "https://t.example/u/TOKEN".to_owned(),
            "https://T.example".to_owned(),
            long,
        ] {
            let html = format!("<a href=\"{href}\">x</a>");
            assert_eq!(
                rewrite(&html, &tracking(&keys, false, true)).unwrap(),
                html,
                "{href}"
            );
        }
        let marked = "<a data-norbelys-no-track href=\"https://example.com\">x</a>";
        assert_eq!(
            rewrite(marked, &tracking(&keys, false, true)).unwrap(),
            marked
        );
        let elsewhere = "<a href=\"https://t.example.evil.com/x\">x</a>";
        assert_ne!(
            rewrite(elsewhere, &tracking(&keys, false, true)).unwrap(),
            elsewhere
        );
    }

    /// The pixel is the last thing in the body (inside `<body>` when there is one) and names
    /// the message; with opens off nothing is added, and with clicks off links stay.
    #[test]
    fn the_pixel_ends_the_body() {
        let keys = keys();
        let tracking = tracking(&keys, true, false);
        let rewritten = rewrite(
            "<html><body><a href=\"https://example.com\">x</a></body></html>",
            &tracking,
        )
        .unwrap();
        assert!(
            rewritten
                .contains("<a href=\"https://example.com\">x</a><img src=\"https://t.example/t/o/")
        );
        assert!(
            rewritten.ends_with("height:1px\"></body></html>"),
            "{rewritten}"
        );
        let token = rewritten
            .split("/t/o/")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        assert_eq!(
            Token::decode(&keys, token),
            Ok(Token::Open {
                workspace: tracking.workspace,
                message: tracking.message
            })
        );
        let untouched = "<p><a href=\"https://example.com\">x</a></p>";
        assert_eq!(
            rewrite(untouched, &self::tracking(&keys, false, false)).unwrap(),
            untouched
        );
    }

    /// Character references decode as a browser reads them; an unknown or broken one stays as
    /// written rather than guessing.
    #[test]
    fn references_decode_as_a_browser_reads_them() {
        for (written, read) in [
            ("a&amp;b", "a&b"),
            ("&lt;&gt;&quot;&apos;", "<>\"'"),
            ("&#38;&#x2F;&#x2f;", "&//"),
            ("a&nbsp;b", "a&nbsp;b"),
            ("a & b", "a & b"),
            ("&#xZZ;", "&#xZZ;"),
            ("trailing&", "trailing&"),
        ] {
            assert_eq!(decode_entities(written), read, "{written}");
        }
    }
}
