//! The frame of the platform's own mail: one layout for every transactional email (sign-in codes,
//! the welcome, invitations, the notices about connections, webhook endpoints and break-glass
//! sessions), written once here, in HTML and in plain text, from the same parts.
//!
//! # An email is data
//!
//! An [`Email`] is a subject, one heading, a body of [`Block`]s, at most one [`Button`] and a
//! quieter closing note, every part a template over the message's `variables`. [`html`] and
//! [`text`] turn it into the two body templates that the creation renders with the subject, like
//! any message's own templates (`rendering::render_own`). No variant writes markup of its own:
//! each kind of part is drawn once, here, and the text version is built from the very templates
//! the HTML is built from, so it carries the same words, with every link written out in full.
//! An email has one button at most because it asks for one action at most, and the type says so.
//!
//! # The HTML
//!
//! Mail clients drop `<style>` elements and much of CSS, so the frame is tables with inline
//! styles: an outer table of `#F4F4F5`; a white card up to 600 pixels wide, bordered and rounded,
//! with 32 pixels of padding (Outlook on Windows ignores `max-width`, so conditional comments give
//! it a fixed 600-pixel table); in the card the mark with the word "norbelys" as live text, the
//! heading, the blocks, the button, the note, and the footer above a rule. Spacing is padding on
//! table cells, never margins, which some clients drop. The button is a table cell of ink pink
//! around a padded link: Outlook ignores the padding of a link, so `mso-padding-alt` gives the
//! cell the same padding there, and the button keeps its size and colour (only its corners turn
//! square). Fonts are the system's own, Inter where it is installed, because mail clients load no
//! web fonts.
//!
//! # Inline markup
//!
//! A text template may hold three tags of its own: `<b>` around a value that matters (an address,
//! a workspace), `<code>` around something to type, and `<a href="…">…</a>` whose text is its own
//! address. The HTML gives them their styles; the text version drops them, `<code>` becoming
//! backticks, so a link's address is always there in full. Nothing else in a template is markup,
//! and every value printed into the HTML is escaped by the template engine.
//!
//! # The mark
//!
//! The mark is the PNG the tracking role serves from the binary
//! ([`EMAIL_MARK_PATH`]): 96 pixels, shown at 24. The role creating a message may not know the
//! tracking host's origin (the worker, which sends the notices, knows none), so the frame links
//! the mark on the stand-in origin [`TRACKING_ORIGIN_STAND_IN`], which the sender replaces with
//! the tracking origin when it prepares the message for its transport.

use super::TRACKING_ORIGIN_STAND_IN;
use crate::tracking::images::EMAIL_MARK_PATH;

/// The text font: the system's own, Inter where it is installed.
const SANS: &str = "Inter,-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
/// The font of codes and things to type.
const MONO: &str = "ui-monospace,SFMono-Regular,Menlo,Consolas,monospace";
/// The heading, the word "norbelys" and what is bold.
const INK: &str = "#0A0A0A";
/// The body text.
const TEXT: &str = "#3F3F46";
/// The quieter note and the lines under a row's name.
const MUTED: &str = "#71717A";
/// The footer.
const FAINT: &str = "#A1A1AA";
/// Rules between rows and above the footer, and the code's border.
const RULE: &str = "#ECECEE";
/// Outside the card, and behind inline code.
const PAGE: &str = "#F4F4F5";
/// The card's border.
const BORDER: &str = "#E7E7EA";
/// Ink pink: the button, and links in the body.
const PINK: &str = "#DB2777";
/// Behind the sign-in code.
const CODE_FILL: &str = "#F6F6F7";
/// The footer's words before the site's name.
const FOOTER: &str = "Norbelys · Open-source outbound email · ";
/// The site the footer names.
const SITE: &str = "norbelys.com";
/// The site's address: the footer's link, written out in full in the text version.
const SITE_URL: &str = "https://norbelys.com";

/// One transactional email, every part a template over the message's `variables`.
#[derive(Debug, Clone, Copy)]
pub struct Email {
    /// The subject: one line of plain text.
    pub subject: &'static str,
    /// The one heading, at the top of the body.
    pub heading: &'static str,
    /// The body, in reading order.
    pub body: &'static [Block],
    /// The one button, after the body, when the email asks for an action.
    pub button: Option<Button>,
    /// The quieter last word, with inline markup: what to do if the email was not expected, or
    /// what a notice means.
    pub note: &'static str,
}

/// The one button of an email.
#[derive(Debug, Clone, Copy)]
pub struct Button {
    /// What it says.
    pub label: &'static str,
    /// Where it leads; the text version writes it out in full under the label.
    pub href: &'static str,
}

/// A part of an email's body.
#[derive(Debug, Clone, Copy)]
pub enum Block {
    /// A paragraph, with inline markup.
    Text(&'static str),
    /// Something to type, large and spaced in a box of its own: the sign-in code.
    Code(&'static str),
    /// A numbered list: each step's lead, in bold, then the rest of it, with inline markup.
    Steps(&'static [(&'static str, &'static str)]),
    /// One row for each item of a list among the variables.
    Rows {
        /// The loop over the items, as a `for` names it: `connection in variables.connections`.
        each: &'static str,
        /// The item's name, in bold.
        title: &'static str,
        /// The quieter line under the name.
        note: &'static str,
        /// Where the item stands, on the right.
        status: &'static str,
        /// An expression of the item whose value, when it has one, is said after the note (a
        /// provider's own words, for example).
        detail: &'static str,
    },
}

/// How loud a paragraph is: the body's text, or the closing note.
#[derive(Debug, Clone, Copy)]
enum Tone {
    Body,
    Muted,
}

/// The HTML body template of `email`: the frame around its parts (see the module).
#[must_use]
pub fn html(email: &Email) -> String {
    let mut rows = cell(0, 24, &header());
    rows.push_str(&cell(
        0,
        8,
        &format!(
            "<h1 style=\"margin:0;font-family:{SANS};font-size:20px;line-height:28px;font-weight:600;color:{INK};mso-line-height-rule:exactly\">{}</h1>",
            email.heading
        ),
    ));
    for block in email.body {
        rows.push_str(&block_html(block));
    }
    if let Some(button) = &email.button {
        rows.push_str(&cell(4, 12, &button_html(button)));
    }
    rows.push_str(&cell(6, 0, &paragraph(email.note, Tone::Muted)));
    rows.push_str(&cell(22, 0, &footer()));
    document(email.subject, &rows)
}

/// The text body template of `email`: its parts in the same order and the same words, inline
/// markup dropped, every link written out in full, and the footer.
#[must_use]
pub fn text(email: &Email) -> String {
    let mut parts = vec![plain(email.heading)];
    parts.extend(email.body.iter().map(block_text));
    if let Some(button) = &email.button {
        parts.push(format!("{}:\n{}", button.label, button.href));
    }
    parts.push(plain(email.note));
    // "-- " (with its space) is the signature separator of plain-text mail (RFC 3676), which
    // clients show as the message's quiet end.
    parts.push(format!("-- \n{FOOTER}{SITE_URL}"));
    parts.join("\n\n")
}

/// The whole document around the card's `rows`.
fn document(subject: &str, rows: &str) -> String {
    format!(
        "<!DOCTYPE html>
<html lang=\"en\" dir=\"ltr\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<meta name=\"x-apple-disable-message-reformatting\">
<meta name=\"color-scheme\" content=\"light\">
<meta name=\"supported-color-schemes\" content=\"light\">
<title>{subject}</title>
</head>
<body style=\"margin:0;padding:0;background-color:{PAGE};-webkit-text-size-adjust:100%;-ms-text-size-adjust:100%\">
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" bgcolor=\"{PAGE}\" style=\"background-color:{PAGE}\">
<tr>
<td align=\"center\" style=\"padding:24px 16px\">
<!--[if mso]><table role=\"presentation\" align=\"center\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr><td><![endif]-->
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" bgcolor=\"#FFFFFF\" style=\"width:100%;max-width:600px;background-color:#FFFFFF;border:1px solid {BORDER};border-radius:6px;border-collapse:separate\">
<tr>
<td style=\"padding:32px;text-align:left\">
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\">
{rows}</table>
</td>
</tr>
</table>
<!--[if mso]></td></tr></table><![endif]-->
</td>
</tr>
</table>
</body>
</html>
"
    )
}

/// One row of the card: `content` with `top` and `bottom` pixels of padding.
fn cell(top: u8, bottom: u8, content: &str) -> String {
    format!("<tr><td style=\"padding:{top}px 0 {bottom}px 0\">{content}</td></tr>\n")
}

/// The mark and the word "norbelys", side by side.
fn header() -> String {
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
<td style=\"padding:0 8px 0 0;vertical-align:middle\"><img src=\"{TRACKING_ORIGIN_STAND_IN}{EMAIL_MARK_PATH}\" width=\"24\" height=\"24\" alt=\"Norbelys\" style=\"display:block;width:24px;height:24px;border:0;outline:none;text-decoration:none\"></td>\
<td style=\"vertical-align:middle;font-family:{SANS};font-size:18px;line-height:24px;font-weight:600;color:{INK};mso-line-height-rule:exactly\">norbelys</td>\
</tr></table>"
    )
}

/// The footer, above its rule.
fn footer() -> String {
    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
<td style=\"padding:14px 0 0 0;border-top:1px solid {RULE};font-family:{SANS};font-size:11px;line-height:16px;color:{FAINT};mso-line-height-rule:exactly\">{FOOTER}<a href=\"{SITE_URL}\" style=\"color:{FAINT};text-decoration:underline\">{SITE}</a></td>\
</tr></table>"
    )
}

/// The button: a cell of ink pink around a padded link (see the module).
fn button_html(button: &Button) -> String {
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
<td align=\"center\" bgcolor=\"{PINK}\" style=\"background-color:{PINK};border-radius:3px;mso-padding-alt:10px 18px\">\
<a href=\"{}\" target=\"_blank\" style=\"display:inline-block;padding:10px 18px;font-family:{SANS};font-size:14px;line-height:20px;font-weight:600;color:#FFFFFF;text-decoration:none;border-radius:3px;mso-line-height-rule:exactly\">{}</a>\
</td></tr></table>",
        button.href, button.label
    )
}

/// One block of the body, as a row of the card.
fn block_html(block: &Block) -> String {
    match block {
        Block::Text(text) => cell(0, 12, &paragraph(text, Tone::Body)),
        // Letter-spacing also follows the last digit, which pulls centred digits to the left:
        // the same space on the left puts them back in the middle.
        Block::Code(code) => cell(
            4,
            18,
            &format!(
                "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
<td align=\"center\" bgcolor=\"{CODE_FILL}\" style=\"padding:14px 0 14px 8px;background-color:{CODE_FILL};border:1px solid {RULE};border-radius:6px;font-family:{MONO};font-size:28px;line-height:40px;font-weight:600;letter-spacing:.28em;color:{INK};text-align:center;mso-line-height-rule:exactly\">{code}</td>\
</tr></table>"
            ),
        ),
        Block::Steps(steps) => {
            let items: String = steps
                .iter()
                .map(|(lead, rest)| {
                    format!(
                        "<li style=\"margin:0 0 6px 20px;padding:0 0 0 4px\"><strong style=\"font-weight:600;color:{INK}\">{lead}</strong> {}</li>",
                        inline(rest, Tone::Body)
                    )
                })
                .collect();
            cell(
                0,
                12,
                &format!(
                    "<ol style=\"margin:0;padding:0;font-family:{SANS};font-size:14px;line-height:22px;color:{TEXT};mso-line-height-rule:exactly\">{items}</ol>"
                ),
            )
        }
        // A line break between the cells keeps their words apart wherever the HTML is read as
        // text (a client's preview, the text derived from HTML).
        Block::Rows {
            each,
            title,
            note,
            status,
            detail,
        } => cell(
            0,
            16,
            &format!(
                "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"border-bottom:1px solid {RULE}\">\n\
{{% for {each} %}}<tr>\n\
<td valign=\"top\" style=\"padding:10px 12px 10px 0;border-top:1px solid {RULE};font-family:{SANS};font-size:13px;line-height:20px;color:{INK};mso-line-height-rule:exactly\"><strong style=\"font-weight:600\">{title}</strong><br><span style=\"font-size:12px;line-height:18px;color:{MUTED}\">{note}{{% if {detail} %}}<br>{{{{ {detail} }}}}{{% endif %}}</span></td>\n\
<td valign=\"top\" align=\"right\" style=\"padding:10px 0;border-top:1px solid {RULE};font-family:{SANS};font-size:12px;line-height:20px;color:{TEXT};text-align:right;mso-line-height-rule:exactly\">{status}</td>\n\
</tr>\n{{% endfor %}}</table>"
            ),
        ),
    }
}

/// One block of the body, as plain text.
fn block_text(block: &Block) -> String {
    match block {
        Block::Text(text) => plain(text),
        Block::Code(code) => (*code).to_owned(),
        Block::Steps(steps) => steps
            .iter()
            .zip(1_u32..)
            .map(|((lead, rest), number)| format!("{number}. {} {}", plain(lead), plain(rest)))
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Rows {
            each,
            title,
            note,
            status,
            detail,
        } => format!(
            "{{% for {each} %}}{{% if not loop.first %}}\n{{% endif %}}- {title} ({note}): {status}{{% if {detail} %}}. {{{{ {detail} }}}}{{% endif %}}{{% endfor %}}"
        ),
    }
}

/// A paragraph of `tone`, its inline markup styled.
fn paragraph(source: &str, tone: Tone) -> String {
    let (size, color) = match tone {
        Tone::Body => ("font-size:14px;line-height:22px", TEXT),
        Tone::Muted => ("font-size:12px;line-height:18px", MUTED),
    };
    format!(
        "<p style=\"margin:0;font-family:{SANS};{size};color:{color};mso-line-height-rule:exactly\">{}</p>",
        inline(source, tone)
    )
}

/// `source` with its inline markup given the styles of `tone` (see the module).
fn inline(source: &str, tone: Tone) -> String {
    let (strong, link) = match tone {
        Tone::Body => (INK, PINK),
        Tone::Muted => (TEXT, MUTED),
    };
    source
        .replace(
            "<b>",
            &format!("<strong style=\"font-weight:600;color:{strong}\">"),
        )
        .replace("</b>", "</strong>")
        .replace(
            "<code>",
            &format!(
                "<code style=\"font-family:{MONO};font-size:13px;color:{TEXT};background-color:{PAGE};border:1px solid #E4E4E7;border-radius:3px;padding:1px 4px\">"
            ),
        )
        .replace(
            "<a href=",
            &format!("<a style=\"color:{link};text-decoration:underline\" href="),
        )
}

/// `source` without its inline markup: `<b>` and links dropped (a link's text is its address),
/// `<code>` turned into backticks. Anything else between angle brackets is kept as written.
fn plain(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some((before, after)) = rest.split_once('<') {
        out.push_str(before);
        let Some((tag, tail)) = after.split_once('>') else {
            out.push('<');
            rest = after;
            break;
        };
        match tag {
            "b" | "/b" | "/a" => {}
            "code" | "/code" => out.push('`'),
            link if link.starts_with("a ") => {}
            other => {
                out.push('<');
                out.push_str(other);
                out.push('>');
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inline markup is styled in the HTML and gone from the text: bold and links leave their
    /// words (a link's text being its address), code keeps backticks, and anything else between
    /// angle brackets stays as written, so nothing a template says is lost on the way.
    #[test]
    fn inline_markup_is_styled_in_html_and_dropped_in_text() {
        let source = "For <b>ada@example.com</b>: run <code>norbelys login</code>, see \
                      <a href=\"https://docs.norbelys.com\">https://docs.norbelys.com</a>; 1 < 2.";
        assert_eq!(
            plain(source),
            "For ada@example.com: run `norbelys login`, see https://docs.norbelys.com; 1 < 2."
        );
        let html = inline(source, Tone::Body);
        assert!(
            html.contains(
                "<strong style=\"font-weight:600;color:#0A0A0A\">ada@example.com</strong>"
            ),
            "{html}"
        );
        assert!(html.contains("<a style=\"color:#DB2777;text-decoration:underline\" href=\"https://docs.norbelys.com\">"), "{html}");
        assert!(html.contains("norbelys login</code>"), "{html}");
    }
}
