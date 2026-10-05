//! The redactor: what is left of an inbound message's body before an excerpt of it may be sent
//! to an AI provider.
//!
//! An excerpt sent to classify a reply keeps what tells a human reply from an auto reply or an
//! out-of-office notice (the words, the dates, the tone) and loses what identifies people or
//! leads to them:
//!
//! - **quoted earlier messages** are cut: everything from the first attribution line ("On …
//!   wrote:", and its German, French, Spanish, Italian, Dutch and Portuguese forms), the first
//!   separator of an original or forwarded message, or the first quoted header block (`From:`
//!   followed by `Sent:`, `Date:` or `To:`, as Outlook writes it); every other line starting
//!   with `>` is dropped, so an answer written between quoted lines stays;
//! - **email addresses** become `[email]`, including the obfuscated `jane [at] example [dot]
//!   com` and `jane at example dot com`;
//! - **URLs** become `[url]`: with a scheme (`https:`, `ftp:`, `mailto:`, `tel:` …), starting
//!   with `www.`, or a bare host name on a common top-level domain (`example.com/pricing`);
//! - **phone numbers** become `[phone]`: runs of 7 to 15 digits with the separators people
//!   write (spaces, dots, dashes, slashes, an area code in parentheses, a leading `+`), but not
//!   dates such as `2026-10-01` or `01.10.2026`.
//!
//! The rules prefer removing too much over too little: an order number may read as a phone
//! number and is removed with it. Truncation is not redaction: the excerpt is first cut at a
//! word boundary well beyond its length, redacted, and only then shortened, so an address that
//! straddles the limit is never left half-visible.
//!
//! Everything here is pure: the same text always gives the same excerpt.

use std::sync::LazyLock;

use regex::Regex;

/// The most of a body read at all: a quoted thread can be long, and nothing beyond this point
/// could reach an excerpt anyway.
const READ_MAX_BYTES: usize = 64 * 1024;

/// An excerpt ready to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt {
    /// The redacted text, at most the asked number of characters.
    pub text: String,
    /// Whether text was left out for length (quoted earlier messages do not count).
    pub truncated: bool,
}

/// Compiles one of this module's constant patterns. Every pattern is exercised by the tests
/// below, so one that does not compile never ships; a pattern that silently matched nothing
/// instead would stop redacting, which is worse than failing.
fn pattern(source: &str) -> Regex {
    Regex::new(source)
        .unwrap_or_else(|error| unreachable!("the redaction pattern compiles: {error}"))
}

/// A line that introduces a quoted earlier message, in the languages of the mail clients most
/// customers' recipients use.
static ATTRIBUTION: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)^(?:",
        r"on\b.+\bwrote\s?:",
        r"|am\b.+\bschrieb\b.*:",
        r"|le\b.+\ba\s+écrit\s?:",
        r"|el\b.+\bescribió\s?:",
        r"|il\b.+\bha\s+scritto\s?:",
        r"|op\b.+\bschreef\b.*:",
        r"|em\b.+\bescreveu\s?:",
        r"|-{2,}\s*(?:original message|ursprüngliche nachricht|message d'origine|mensaje original|messaggio originale|oorspronkelijk bericht|mensagem original|forwarded message|weitergeleitete nachricht|message transféré|mensaje reenviado)\s*-{2,}",
        r"|begin forwarded message\s?:",
        r")$",
    ))
});

/// The first line of a quoted header block, as Outlook writes it above an earlier message.
static HEADER_FROM: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)^\*?(?:from|von|de|da|van|fra|från)\*?\s*:\s*\S"));

/// A following line of a quoted header block.
static HEADER_NEXT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)^\*?(?:sent|date|to|subject|gesendet|datum|an|betreff|envoyé|date|à|objet|enviado|fecha|para|asunto|inviato|data|oggetto|verzonden|aan|onderwerp)\*?\s*:",
    )
});

/// Outlook's rule above a quoted header block.
static RULE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^_{10,}$"));

/// A URL with a scheme, or starting with `www.`.
static URL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r#"(?i)\b(?:(?:https?|ftp)://|www\.|(?:mailto|tel|sms|callto|skype):)[^\s<>"'`]+"#)
});

/// An email address, Unicode local parts and domains included.
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)[\p{L}\p{N}._%+'-]+@(?:[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,61}[\p{L}\p{N}])?\.)+\p{L}{2,}",
    )
});

/// An email address written to escape harvesters: `jane [at] example [dot] com`,
/// `jane(at)example.com`, `jane at example dot com`.
static OBFUSCATED_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i)[\p{L}\p{N}._%+-]+\s*(?:\[at\]|\(at\)|\{at\}|<at>)\s*[\p{L}\p{N}-]+(?:\s*(?:\[dot\]|\(dot\)|\{dot\}|<dot>|\.)\s*[\p{L}\p{N}-]+)+",
        r"|\b[\p{L}\p{N}._%+-]+\s+at\s+[\p{L}\p{N}-]+(?:\s+dot\s+[\p{L}\p{N}-]+)+\b",
    ))
});

/// A bare host name on a common top-level domain, with its path: `example.com/pricing`.
static HOST: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r#"(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+"#,
        r"(?:com|net|org|io|co|ai|app|dev|info|biz|me|us|uk|de|fr|es|it|nl|be|ch|at|eu|se|no|dk|fi|pl|pt|br|mx|ca|au|nz|in|jp|cn|ru|tv|ly|so|xyz|tech|site|online|store|shop|cloud|example)\b",
        r#"(?:/[^\s<>"'`]*)?"#,
    ))
});

/// A candidate phone number: an optional `+` and country code, an optional area code in
/// parentheses, then digits and separators; [`is_phone`] decides.
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?:\+[0-9]{1,3}[ .\-]?(?:\(0?[0-9]{1,5}\)[ .\-]?)?|\([0-9]{1,5}\)[ .\-]?|\b)",
        r"[0-9][0-9 .\-/]{4,}[0-9]\b",
    ))
});

/// A date inside a candidate phone number: the candidate is a date, not a number to remove.
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"[0-9]{4}[-./][0-9]{1,2}[-./][0-9]{1,2}|[0-9]{1,2}[-./][0-9]{1,2}[-./][0-9]{2,4}")
});

/// Characters that end a sentence rather than a URL: kept outside the replacement.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"', '>'];

/// The excerpt of `body` that may be sent: quoted earlier messages cut, identifiers replaced,
/// blank lines collapsed, at most `max_chars` characters.
#[must_use]
pub fn excerpt(body: &str, max_chars: usize) -> Excerpt {
    let read = prefix_bytes(body, READ_MAX_BYTES);
    let normalized = read.replace("\r\n", "\n").replace('\r', "\n");
    let unquoted = cut_quotes(&normalized);
    // Cut well beyond the limit at a word boundary first, so nothing is redacted half.
    let (bounded, mut truncated) = prefix_words(&unquoted, max_chars.saturating_mul(4));
    truncated |= read.len() < body.len();
    let redacted = tidy(&redact(bounded));
    let text = if redacted.chars().count() > max_chars {
        truncated = true;
        redacted.chars().take(max_chars).collect::<String>()
    } else {
        redacted
    };
    Excerpt {
        text: text.trim_end().to_owned(),
        truncated,
    }
}

/// `text` with every email address, URL and phone number replaced by `[email]`, `[url]` and
/// `[phone]`.
#[must_use]
pub fn redact(text: &str) -> String {
    let text = replace_trimmed(&URL, text, "[url]");
    let text = EMAIL.replace_all(&text, "[email]");
    let text = OBFUSCATED_EMAIL.replace_all(&text, "[email]");
    let text = replace_trimmed(&HOST, &text, "[url]");
    PHONE
        .replace_all(&text, |captures: &regex::Captures<'_>| {
            let found = captures.get(0).map_or("", |found| found.as_str());
            if is_phone(found) {
                "[phone]".to_owned()
            } else {
                found.to_owned()
            }
        })
        .into_owned()
}

/// Whether `text` holds an email address, a URL or a phone number: what a text written by a
/// model for a customer's mail must never invent.
#[must_use]
pub fn holds_contact(text: &str) -> bool {
    URL.is_match(text)
        || EMAIL.is_match(text)
        || OBFUSCATED_EMAIL.is_match(text)
        || HOST.is_match(text)
        || PHONE.find_iter(text).any(|found| is_phone(found.as_str()))
}

/// Whether a candidate is a phone number: 7 to 15 digits (the E.164 maximum), and no date in
/// it.
fn is_phone(candidate: &str) -> bool {
    let digits = candidate.bytes().filter(u8::is_ascii_digit).count();
    (7..=15).contains(&digits) && !DATE.is_match(candidate)
}

/// Replaces each match of `pattern` in `text` with `with`, keeping the punctuation that ends a
/// sentence outside it: `see example.com/a.` keeps its full stop.
fn replace_trimmed(pattern: &Regex, text: &str, with: &str) -> String {
    pattern
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let found = captures.get(0).map_or("", |found| found.as_str());
            let kept = found.trim_end_matches(TRAILING);
            format!("{with}{}", &found[kept.len()..])
        })
        .into_owned()
}

/// `text` from its start to the first line that introduces a quoted earlier message, without
/// the other lines that start with `>`.
fn cut_quotes(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = (0..lines.len())
        .find(|&index| quote_starts(&lines, index))
        .unwrap_or(lines.len());
    lines
        .iter()
        .take(end)
        .filter(|line| !line.trim_start().starts_with('>'))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a quoted earlier message starts at line `index`: an attribution, also one a client
/// wrapped over two lines; a quoted header block; or Outlook's rule above one.
fn quote_starts(lines: &[&str], index: usize) -> bool {
    let line = |at: usize| lines.get(at).map_or("", |line| line.trim());
    let here = line(index);
    if here.is_empty() {
        return false;
    }
    if ATTRIBUTION.is_match(here) {
        return true;
    }
    let next = line(index + 1);
    if !next.is_empty() && ATTRIBUTION.is_match(&format!("{here} {next}")) {
        return true;
    }
    let header_block = |from: usize| {
        HEADER_FROM.is_match(line(from))
            && (from + 1..=from + 4).any(|at| HEADER_NEXT.is_match(line(at)))
    };
    header_block(index) || (RULE.is_match(here) && (index + 1..=index + 2).any(header_block))
}

/// The longest prefix of `text` of at most `max` bytes that ends on a character boundary.
fn prefix_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The prefix of `text` of at most `max` characters, cut back to the last whitespace when it
/// had to be cut, and whether it was.
fn prefix_words(text: &str, max: usize) -> (&str, bool) {
    let Some((end, _)) = text.char_indices().nth(max) else {
        return (text, false);
    };
    let cut = &text[..end];
    let at_word = cut
        .rfind(char::is_whitespace)
        .map_or(cut, |space| &cut[..space]);
    (at_word, true)
}

/// Trailing spaces removed from every line, runs of blank lines collapsed to one, and the
/// whole trimmed.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{excerpt, holds_contact, redact};

    /// Email addresses of every common shape become `[email]`: plain, with plus addressing,
    /// subdomains, capitals, Unicode, in angle brackets, as a `mailto:` link, and the forms
    /// people write to escape harvesters.
    #[test]
    fn email_addresses_are_removed() {
        for (text, expected) in [
            (
                "write to jane.doe@example.com today",
                "write to [email] today",
            ),
            ("jane+sales@mail.example.co.uk", "[email]"),
            ("JANE.DOE@EXAMPLE.COM", "[email]"),
            ("josé.müller@exämple.de", "[email]"),
            ("Jane Doe <jane@example.com>", "Jane Doe <[email]>"),
            ("o'brien@example.ie", "[email]"),
            ("mailto:jane@example.com", "[url]"),
            ("jane [at] example [dot] com", "[email]"),
            ("jane(at)example.com", "[email]"),
            ("reach me: jane at example dot com", "reach me: [email]"),
        ] {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    /// URLs with a scheme, starting with `www.`, or bare host names on common top-level domains
    /// become `[url]`; the punctuation that ends the sentence stays outside, and words that
    /// merely contain dots (`e.g.`, `Node.js`, `v4.5`) are left alone.
    #[test]
    fn urls_are_removed() {
        for (text, expected) in [
            (
                "Book here: https://cal.example.com/jane?week=42.",
                "Book here: [url].",
            ),
            ("(see http://example.org/a)", "(see [url])"),
            ("www.example.com/pricing, or call", "[url], or call"),
            ("visit acme-robotics.com/about!", "visit [url]!"),
            ("our site acme.co.uk", "our site [url]"),
            ("ftp://files.example.net/x.pdf", "[url]"),
            ("tel:+15550109999", "[url]"),
            ("e.g. Node.js v4.5 works", "e.g. Node.js v4.5 works"),
        ] {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    /// Phone numbers in the forms people write them become `[phone]`, with their country code
    /// and area code; dates, times, short numbers, prices and version numbers stay, because they
    /// tell an out-of-office notice from a reply.
    #[test]
    fn phone_numbers_are_removed_and_dates_kept() {
        for (text, expected) in [
            ("call +1 (555) 010-9999 now", "call [phone] now"),
            ("+44 20 7946 0958", "[phone]"),
            ("0049 30 12345678", "[phone]"),
            ("(030) 1234 5678", "[phone]"),
            ("555.010.1234", "[phone]"),
            ("+49 (0)30 1234567", "[phone]"),
            ("Tel.: 030/1234567", "Tel.: [phone]"),
            ("Mobile +33 1 23 45 67 89.", "Mobile [phone]."),
            ("5550109999", "[phone]"),
        ] {
            assert_eq!(redact(text), expected, "{text}");
        }
        for kept in [
            "back on 2026-10-12",
            "back on 12.10.2026",
            "back on 10/12/2026 at 10:30",
            "3 items for $1,299.00",
            "version 4.5.1 in Q3 2026",
            "room 1204",
        ] {
            assert_eq!(redact(kept), kept, "{kept}");
        }
    }

    /// Everything from an attribution line on is cut, in each language the patterns know and
    /// when a client wrapped the attribution over two lines; what came before stays.
    #[test]
    fn quoted_messages_are_cut_at_their_attribution() {
        for attribution in [
            "On Mon, Oct 5, 2026 at 10:02 AM Jane Doe <jane@example.com> wrote:",
            "On Mon, Oct 5, 2026 at 10:02 AM Jane Doe <\njane@example.com> wrote:",
            "Am Mo., 5. Okt. 2026 um 10:02 Uhr schrieb Jane Doe <jane@example.com>:",
            "Le lun. 5 oct. 2026 à 10:02, Jane Doe <jane@example.com> a écrit :",
            "El lun, 5 oct 2026 a las 10:02, Jane Doe (<jane@example.com>) escribió:",
            "Il giorno lun 5 ott 2026 alle ore 10:02 Jane Doe <jane@example.com> ha scritto:",
            "Op ma 5 okt. 2026 om 10:02 schreef Jane Doe <jane@example.com>:",
            "Em seg., 5 de out. de 2026 às 10:02, Jane Doe <jane@example.com> escreveu:",
            "-----Original Message-----",
            "-----Ursprüngliche Nachricht-----",
            "---------- Forwarded message ---------",
            "Begin forwarded message:",
        ] {
            let body = format!(
                "Thanks, Tuesday works for me.\n\n{attribution}\n> Would you like a demo?\nSecret line"
            );
            assert_eq!(
                excerpt(&body, 500).text,
                "Thanks, Tuesday works for me.",
                "{attribution}"
            );
        }
    }

    /// Outlook's quoted header block (`From:` then `Sent:` or `Date:` a few lines below), with
    /// or without its rule above it, starts the quote; a lone `From:` sentence does not.
    #[test]
    fn quoted_header_blocks_are_cut() {
        let outlook = "Sounds good.\n\n________________________________\nFrom: Jane Doe <jane@example.com>\nSent: Monday, October 5, 2026 10:02\nTo: Max\nSubject: Demo\n\nHi Max";
        assert_eq!(excerpt(outlook, 500).text, "Sounds good.");
        let german = "Gerne.\nVon: Jane Doe\nGesendet: Montag, 5. Oktober 2026 10:02\nAn: Max";
        assert_eq!(excerpt(german, 500).text, "Gerne.");
        let sentence = "From: my side, all good.\nLet us talk next week.";
        assert_eq!(excerpt(sentence, 500).text, sentence);
    }

    /// An answer written between quoted lines keeps its own lines and loses the quoted ones.
    #[test]
    fn interleaved_quotes_are_dropped_and_answers_kept() {
        let body =
            "Hi,\n> Would you like a demo?\nYes, Tuesday works.\n> Pricing?\nToo expensive for us.";
        assert_eq!(
            excerpt(body, 500).text,
            "Hi,\nYes, Tuesday works.\nToo expensive for us."
        );
    }

    /// An excerpt is at most its length, marked truncated when text was left out, and an
    /// address straddling the limit is never left half-visible: the text is cut at a word
    /// boundary beyond the limit and redacted before it is shortened.
    #[test]
    fn excerpts_are_bounded_after_redaction() {
        let body = format!("{} jane.doe@example.com and more", "word ".repeat(10));
        let short = excerpt(&body, 60);
        assert!(short.truncated);
        assert!(short.text.chars().count() <= 60);
        assert!(!short.text.contains("jane"), "{}", short.text);
        let whole = excerpt("A short reply.", 500);
        assert_eq!(whole.text, "A short reply.");
        assert!(!whole.truncated);
        let long = "x".repeat(200_000);
        assert!(excerpt(&long, 100).truncated);
    }

    /// Line endings are normalised, trailing spaces dropped and runs of blank lines collapsed,
    /// so the excerpt spends its length on words.
    #[test]
    fn whitespace_is_tidied() {
        assert_eq!(
            excerpt("Hello   \r\n\r\n\r\n\r\nThanks\rBye  ", 500).text,
            "Hello\n\nThanks\nBye"
        );
    }

    /// The check for invented contact data finds each kind of identifier the redactor removes,
    /// and nothing in ordinary prose.
    #[test]
    fn contact_data_is_detected() {
        for text in [
            "mail jane@example.com",
            "see https://example.com",
            "visit example.com",
            "call +1 555 010 9999",
            "jane [at] example [dot] com",
        ] {
            assert!(holds_contact(text), "{text}");
        }
        for text in [
            "Congrats on the Series B, Ada!",
            "Your team at Acme grew 40% in 2026.",
            "We met on 2026-09-30.",
        ] {
            assert!(!holds_contact(text), "{text}");
        }
    }
}
