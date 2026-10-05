//! Tracking decisions: who an open or a click came from, whether it is recorded, and what the
//! tracking routes answer. Every function here is pure: the request's facts, the message's age
//! and `now` arrive as arguments.
//!
//! # The actor class
//!
//! A tracking event is an open (a message's pixel was fetched) or a click (one of its links was
//! followed). Many of them are not a person reading the mail, and counting them as one inflates
//! every rate a customer steers by, so each event carries an actor class, read from what the
//! request shows:
//!
//! | Class | Meaning | What shows it |
//! |---|---|---|
//! | `human` | a person's mail client or browser, directly or through a proxy that fetches when the person opens the message | a browser's or a mail client's `User-Agent`; Gmail's and Yahoo's image proxies on an open |
//! | `proxy` | a privacy proxy that fetches every image whether or not the person opens the message | Apple Mail Privacy Protection's prefetch |
//! | `scanner` | software reading the message for someone else: a security gateway, a link checker, a previewer, a crawler | `HEAD`; a crawler's, previewer's or HTTP library's `User-Agent`; any fetch within [`PERSON_DELAY`] of the message's creation |
//! | `unknown` | nothing tells | no `User-Agent`, or one that names no browser, mail client or known automation |
//!
//! Only `human` events reach the campaign counters (`opened` and `clicked` count messages with a
//! human open or click); the per-message rollup keeps every class apart.
//!
//! The evidence for each rule, read on 2026-10-02:
//!
//! - **Apple Mail Privacy Protection** prefetches. Apple: it "stops senders from using invisible
//!   pixels to collect information about the user" and "helps users prevent senders from knowing
//!   when they open an email, and masks their IP address"
//!   (<https://www.apple.com/newsroom/2021/06/apple-advances-its-privacy-leadership-with-ios-15-ipados-15-macos-monterey-and-watchos-8/>,
//!   2021-06-07). Postmark: "When a sender sends an email to an Apple Mail user, Apple caches the
//!   entire email on its own server. When doing so, they download all images, including tracking
//!   pixels" (<https://postmarkapp.com/blog/how-apples-mail-privacy-changes-affect-email-open-tracking>,
//!   2021-11-09). Litmus: "Apple first routes emails through a proxy server to pre-load message
//!   content", from "an IP address assigned to the general region of the subscriber"
//!   (<https://www.litmus.com/blog/apple-mail-privacy-protection-for-marketers>, 2021-09-20), so
//!   the address cannot single the proxy out; its request names no browser, only a bare
//!   `Mozilla/5.0`, which no browser or mail client sends. Twilio SendGrid flags these opens as
//!   machine opens (`sg_machine_open`: "whether or not Apple Mail Privacy Protection (MPP)
//!   generated an open event",
//!   <https://www.twilio.com/docs/sendgrid/for-developers/tracking-events/event>).
//! - **Gmail and Yahoo Mail** show a message's images through their own proxies, so the sender
//!   never learns the reader's address: "Senders can't use image loading to get information about
//!   your computer or location" (Gmail Help, "Turn images on or off in Gmail",
//!   <https://support.google.com/mail/answer/145919>); Yahoo documents its proxy at the address
//!   its `User-Agent` names ("Mail Proxy Servers", <https://help.yahoo.com/kb/SLN28749.html>). Their
//!   fetch happens when the message is shown to the person, so an open through them is the
//!   person's; the proxy caches the image, so their later opens never reach us. A proxy never
//!   follows a link, so a click carrying its `User-Agent` is a machine's.
//! - **Security gateways read mail as it is delivered.** Microsoft Defender for Office 365: "URLs
//!   are scanned prior to message delivery, regardless of whether the URLs are rewritten or not",
//!   and with "Wait for URL scanning to complete before delivering the message" the "messages that
//!   contain URLs are held until scanning is finished"
//!   (<https://learn.microsoft.com/en-us/defender-office-365/safe-links-about>). A person needs
//!   the message submitted, delivered, noticed and opened before anything they do reaches us, so a
//!   fetch within [`PERSON_DELAY`] of the message's creation is such a reader. Only campaign mail
//!   is tracked, and nobody waits for cold mail to arrive.
//! - **`HEAD`** asks for a resource's metadata without its content, "often for the sake of
//!   testing hypertext links" (RFC 9110 §9.3.2,
//!   <https://www.rfc-editor.org/rfc/rfc9110#section-9.3.2>); neither a mail client nor a browser
//!   following a link sends it.
//!
//! The ceiling of a stateless reading: a scanner that waits past [`PERSON_DELAY`] and presents a
//! browser's `User-Agent` counts as a person. Catching it needs what one request cannot show: the
//! addresses scanners fetch from, or a burst of every link of one message within a second, both
//! decided across events rather than per event.
//!
//! # What is recorded
//!
//! An event is recorded only when its token verifies and its message is younger than
//! [`RECORD_WINDOW`], the time a message's raw events and per-message rollup stay online. An older
//! message's rollup has been archived, so its events could no longer be counted; they are still
//! answered (the pixel, the redirect), only not recorded. The window follows the online retention
//! of `tracking_events` and `message_engagement` (30 days, the default); a deployment that keeps
//! them longer raises it with them.
//!
//! # What the routes answer
//!
//! The open route always answers its pixel, whatever the token: a broken image in a recipient's
//! mail helps nobody, and the pixel tells nothing. The click route redirects only to the
//! destination its token signs, and only to an `http` or `https` address; any other token has
//! nowhere to lead and answers `404`, so the route can never be made to redirect anywhere else
//! (an open redirect).

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use uuid::Uuid;

/// How long after its creation a message can first be read by a person: it must be submitted,
/// delivered, noticed and opened first. A fetch sooner than this is a machine's.
pub const PERSON_DELAY: Duration = Duration::from_secs(60);

/// How long a message's events are recorded: the online window of raw tracking events and of the
/// per-message rollup. Later events are answered and not recorded.
pub const RECORD_WINDOW: Duration = Duration::from_secs(30 * 86_400);

/// `User-Agent` tokens of the mail providers' image proxies that fetch when a person opens the
/// message, in lowercase.
const OPEN_PROXIES: [&str; 2] = ["googleimageproxy", "yahoomailproxy"];

/// `User-Agent` fragments of software that reads for no person, in lowercase: crawlers and link
/// previewers (most name themselves `…bot`), headless browsers, HTTP libraries and command-line
/// clients, and Microsoft Office's check that a link exists before opening it. Matched as
/// substrings, so a new name is one entry.
const AUTOMATION: [&str; 31] = [
    "bot/",
    "bot;",
    "bot)",
    "crawler",
    "spider",
    "preview",
    "slackbot",
    "telegrambot",
    "facebookexternalhit",
    "whatsapp",
    "headlesschrome",
    "phantomjs",
    "python-requests",
    "python-urllib",
    "aiohttp",
    "curl/",
    "wget/",
    "go-http-client",
    "java/",
    "okhttp",
    "libwww-perl",
    "apache-httpclient",
    "axios/",
    "node-fetch",
    "undici",
    "scrapy",
    "postmanruntime",
    "existence discovery",
    "barracuda",
    "mimecast",
    "proofpoint",
];

/// `User-Agent` fragments of mail clients whose `User-Agent` does not start the way a browser's
/// does, in lowercase (classic Outlook names Microsoft Office and Outlook).
const MAIL_CLIENTS: [&str; 3] = ["microsoft outlook", "outlook-ios", "outlook-android"];

/// What was observed.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The message's open pixel was fetched.
    Open,
    /// One of the message's links was followed.
    Click,
}

impl EventKind {
    /// The kind as stored (`tracking_events.kind`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Whether a tracking event looks like a person (see the module).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ActorClass {
    /// A person, directly or through a proxy that fetches when the person opens the message.
    Human,
    /// Software reading the message for someone else: a gateway, a link checker, a crawler.
    Scanner,
    /// A privacy proxy that fetches whether or not the person opens the message.
    Proxy,
    /// Nothing tells.
    Unknown,
}

impl ActorClass {
    /// The class as stored (`tracking_events.actor_class`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// How the request fetched the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Fetch {
    /// `GET`: what a mail client and a browser send.
    Get,
    /// `HEAD`: the metadata without the content, as link checkers ask.
    Head,
}

/// What the request's `User-Agent` says the requester is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Agent {
    /// A browser or a mail client.
    Browser,
    /// A mail provider's image proxy that fetches when the person opens the message (Gmail,
    /// Yahoo).
    OpenProxy,
    /// A bare `Mozilla/5.0`, naming no engine or platform: the form of Apple Mail Privacy
    /// Protection's prefetch, and of no browser.
    Bare,
    /// A crawler, a previewer, a headless browser, an HTTP library or a link checker.
    Automation,
    /// Something else, which names no browser, mail client or known automation.
    Other,
    /// No `User-Agent`.
    Missing,
}

impl Agent {
    /// Reads a request's `User-Agent`, ignoring ASCII case.
    #[must_use]
    pub fn of(user_agent: Option<&str>) -> Self {
        let Some(text) = user_agent.map(str::trim).filter(|text| !text.is_empty()) else {
            return Self::Missing;
        };
        let lower = text.to_ascii_lowercase();
        let has = |fragments: &[&str]| fragments.iter().any(|fragment| lower.contains(fragment));
        if has(&OPEN_PROXIES) {
            Self::OpenProxy
        } else if has(&AUTOMATION) {
            Self::Automation
        } else if lower == "mozilla/5.0" {
            Self::Bare
        } else if (lower.starts_with("mozilla/") && lower.contains('(')) || has(&MAIL_CLIENTS) {
            Self::Browser
        } else {
            Self::Other
        }
    }
}

/// How old the message was when the event happened, as the decisions see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Age {
    /// Younger than [`PERSON_DELAY`]: too soon for a person.
    Immediate,
    /// Within [`RECORD_WINDOW`].
    Recent,
    /// Older than [`RECORD_WINDOW`]: answered, not recorded.
    Expired,
}

impl Age {
    /// The age of a message created at `created` (unknown: `None`, read as recent) at `now`. A
    /// creation after `now` (another host's clock) is immediate.
    #[must_use]
    pub fn of(created: Option<Timestamp>, now: Timestamp) -> Self {
        let Some(created) = created else {
            return Self::Recent;
        };
        let elapsed = now.duration_since(created);
        if elapsed < signed(PERSON_DELAY) {
            Self::Immediate
        } else if elapsed > signed(RECORD_WINDOW) {
            Self::Expired
        } else {
            Self::Recent
        }
    }
}

/// When the message with this id was created: a message's id is a UUIDv7, whose first 48 bits
/// are its creation's Unix milliseconds (RFC 9562 §5.7,
/// <https://www.rfc-editor.org/rfc/rfc9562#section-5.7>). `None` for an id of another version.
#[must_use]
pub fn created_at(message: Uuid) -> Option<Timestamp> {
    let (seconds, nanos) = message.get_timestamp()?.to_unix();
    Timestamp::new(i64::try_from(seconds).ok()?, i32::try_from(nanos).ok()?).ok()
}

/// The actor class of one event (see the module's table).
#[must_use]
pub fn classify(kind: EventKind, fetch: Fetch, agent: Agent, age: Age) -> ActorClass {
    match (fetch, agent) {
        (Fetch::Head, _) | (Fetch::Get, Agent::Automation) => ActorClass::Scanner,
        (Fetch::Get, Agent::Missing | Agent::Other) => ActorClass::Unknown,
        (Fetch::Get, Agent::Bare) => match kind {
            EventKind::Open => ActorClass::Proxy,
            EventKind::Click => ActorClass::Scanner,
        },
        (Fetch::Get, Agent::OpenProxy) => match (kind, age) {
            (EventKind::Open, Age::Recent | Age::Expired) => ActorClass::Human,
            (EventKind::Open, Age::Immediate) | (EventKind::Click, _) => ActorClass::Scanner,
        },
        (Fetch::Get, Agent::Browser) => match age {
            Age::Immediate => ActorClass::Scanner,
            Age::Recent | Age::Expired => ActorClass::Human,
        },
    }
}

/// What a route's token turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Check {
    /// Signed by this deployment for this route; for a click, leading to a web address.
    Valid,
    /// Anything else: malformed, altered, signed elsewhere, made for another route, or a click
    /// whose destination is not a web address.
    Refused,
}

/// What a tracking route answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The 1×1 image, never cached.
    Pixel,
    /// `302` to the destination the click's token signs.
    Redirect,
    /// `404`: nowhere to lead.
    NotFound,
}

/// What the route of `kind` answers a token that is `check` (see the module).
#[must_use]
pub fn answer(kind: EventKind, check: Check) -> Answer {
    match (kind, check) {
        (EventKind::Open, Check::Valid | Check::Refused) => Answer::Pixel,
        (EventKind::Click, Check::Valid) => Answer::Redirect,
        (EventKind::Click, Check::Refused) => Answer::NotFound,
    }
}

/// Whether an event whose token is `check` and whose message is `age` old is recorded.
#[must_use]
pub fn recorded(check: Check, age: Age) -> bool {
    match (check, age) {
        (Check::Valid, Age::Immediate | Age::Recent) => true,
        (Check::Valid, Age::Expired) | (Check::Refused, _) => false,
    }
}

/// Whether a click's destination may be redirected to: an absolute `http` or `https` address
/// with a host and nothing a header could not carry (no whitespace, no control characters). The
/// rendering only signs such links; reading them again here means a token can never lead
/// anywhere else, even one signed by a mistake.
#[must_use]
pub fn redirectable(destination: &str) -> bool {
    let lower = destination.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    let host = rest.and_then(|rest| rest.split(['/', '?', '#']).next());
    host.is_some_and(|host| !host.is_empty())
        && destination
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control())
}

/// `duration` as a signed duration, saturating.
fn signed(duration: Duration) -> SignedDuration {
    SignedDuration::try_from(duration).unwrap_or(SignedDuration::MAX)
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;
    use uuid::{NoContext, Uuid};

    use super::*;

    /// The actor class of every combination of kind, fetch, agent and age: a `HEAD` and known
    /// automation are scanners whatever else holds; no or an unrecognised `User-Agent` tells
    /// nothing; Apple's bare prefetch is a proxy on an open and a machine on a click; Gmail's and
    /// Yahoo's proxies open for the person, except within the first minute, and never click; a
    /// browser is a person unless it fetched within the first minute. Generated over the enums, so
    /// a new variant fails here until its class is decided.
    #[test]
    fn every_request_has_its_actor_class() {
        for kind in EventKind::iter() {
            for fetch in Fetch::iter() {
                for agent in Agent::iter() {
                    for age in Age::iter() {
                        let soon = age == Age::Immediate;
                        let expected = match (fetch, agent, kind) {
                            (Fetch::Head, ..) | (Fetch::Get, Agent::Automation, _) => {
                                ActorClass::Scanner
                            }
                            (Fetch::Get, Agent::Missing | Agent::Other, _) => ActorClass::Unknown,
                            (Fetch::Get, Agent::Bare, EventKind::Open) => ActorClass::Proxy,
                            (Fetch::Get, Agent::Bare, EventKind::Click) => ActorClass::Scanner,
                            (Fetch::Get, Agent::OpenProxy, EventKind::Click) => ActorClass::Scanner,
                            (Fetch::Get, Agent::OpenProxy | Agent::Browser, _) if soon => {
                                ActorClass::Scanner
                            }
                            (Fetch::Get, Agent::OpenProxy | Agent::Browser, _) => ActorClass::Human,
                        };
                        assert_eq!(
                            classify(kind, fetch, agent, age),
                            expected,
                            "{kind:?} {fetch:?} {agent:?} {age:?}"
                        );
                    }
                }
            }
        }
    }

    /// Real `User-Agent` strings read as what they are: browsers and mail clients (classic
    /// Outlook included) as browsers, Gmail's and Yahoo's image proxies as open proxies, Apple's
    /// bare prefetch as bare, crawlers, previewers, libraries and Office's link check as
    /// automation, and anything unnamed or absent as other or missing. Case and surrounding space
    /// do not matter.
    #[test]
    fn user_agents_read_as_what_they_are() {
        let cases = [
            (
                Some(
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)",
                ),
                Agent::Browser,
            ),
            (
                Some(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Thunderbird/128.3.0",
                ),
                Agent::Browser,
            ),
            (
                Some("Mozilla/4.0 (compatible; ms-office; MSOffice 16)"),
                Agent::Browser,
            ),
            (
                Some("Microsoft Office/16.0 (Windows NT 10.0; Microsoft Outlook 16.0.17928; Pro)"),
                Agent::Browser,
            ),
            (
                Some(
                    "Mozilla/5.0 (Windows NT 5.1; rv:11.0) Gecko Firefox/11.0 (via ggpht.com GoogleImageProxy)",
                ),
                Agent::OpenProxy,
            ),
            (
                Some("YahooMailProxy; https://help.yahoo.com/kb/yahoo-mail-proxy-SLN28749.html"),
                Agent::OpenProxy,
            ),
            (Some("Mozilla/5.0"), Agent::Bare),
            (Some("  mozilla/5.0 "), Agent::Bare),
            (
                Some("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"),
                Agent::Automation,
            ),
            (
                Some("Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)"),
                Agent::Automation,
            ),
            (Some("facebookexternalhit/1.1"), Agent::Automation),
            (
                Some(
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/120.0.0.0 Safari/537.36",
                ),
                Agent::Automation,
            ),
            (Some("python-requests/2.32.3"), Agent::Automation),
            (Some("curl/8.7.1"), Agent::Automation),
            (Some("Go-http-client/1.1"), Agent::Automation),
            (
                Some("Microsoft Office Existence Discovery"),
                Agent::Automation,
            ),
            (Some("SomethingElse/1.0"), Agent::Other),
            (
                Some("Mozilla/5.0 (Linux; Android 10; CUBOT X30)"),
                Agent::Browser,
            ),
            (Some(""), Agent::Missing),
            (Some("   "), Agent::Missing),
            (None, Agent::Missing),
        ];
        for (user_agent, expected) in cases {
            assert_eq!(Agent::of(user_agent), expected, "{user_agent:?}");
        }
    }

    /// A message's age is immediate up to its first minute (a creation after `now`, another
    /// host's clock, included), recent up to and including the record window, expired beyond it;
    /// an unknown creation is recent, so a request is still classified by what it shows.
    #[test]
    fn ages_have_their_edges() {
        let now = Timestamp::from_second(2_000_000_000).unwrap();
        let ago = |seconds: i64| Some(now.checked_sub(SignedDuration::from_secs(seconds)).unwrap());
        let window = i64::try_from(RECORD_WINDOW.as_secs()).unwrap();
        for (created, expected) in [
            (ago(-5), Age::Immediate),
            (ago(0), Age::Immediate),
            (ago(59), Age::Immediate),
            (ago(60), Age::Recent),
            (ago(window), Age::Recent),
            (ago(window + 1), Age::Expired),
            (None, Age::Recent),
        ] {
            assert_eq!(Age::of(created, now), expected, "{created:?}");
        }
    }

    /// A message id's creation time is read back from its UUIDv7 to the millisecond; an id of
    /// another version has none.
    #[test]
    fn a_message_id_carries_its_creation_time() {
        let at = uuid::Timestamp::from_unix(NoContext, 1_900_000_000, 123_000_000);
        let id = Uuid::new_v7(at);
        assert_eq!(
            created_at(id),
            Some(Timestamp::new(1_900_000_000, 123_000_000).unwrap())
        );
        assert_eq!(created_at(Uuid::nil()), None);
    }

    /// The open route always answers the pixel and the click route redirects only for a valid
    /// token; only a valid token of a message within the record window is recorded. Generated
    /// over the enums.
    #[test]
    fn tokens_decide_the_answer_and_the_record() {
        for kind in EventKind::iter() {
            for check in Check::iter() {
                let expected = match (kind, check) {
                    (EventKind::Open, _) => Answer::Pixel,
                    (EventKind::Click, Check::Valid) => Answer::Redirect,
                    (EventKind::Click, Check::Refused) => Answer::NotFound,
                };
                assert_eq!(answer(kind, check), expected, "{kind:?} {check:?}");
                for age in Age::iter() {
                    let expected = check == Check::Valid && age != Age::Expired;
                    assert_eq!(recorded(check, age), expected, "{check:?} {age:?}");
                }
            }
        }
    }

    /// Only an absolute web address with a host may be redirected to: no other scheme
    /// (`javascript:`, `data:`, `mailto:`), no relative or host-less link, nothing with
    /// whitespace or control characters (a header injection).
    #[test]
    fn only_web_addresses_are_redirectable() {
        for good in [
            "https://example.com",
            "HTTP://Example.com/a?b=1#c",
            "https://example.com:8443/path",
        ] {
            assert!(redirectable(good), "{good}");
        }
        for bad in [
            "javascript:alert(1)",
            "data:text/html,hi",
            "mailto:a@example.com",
            "/relative",
            "https://",
            "https:///path",
            "//example.com",
            "https://example.com/a b",
            "https://example.com/\r\nSet-Cookie: x=1",
            "",
        ] {
            assert!(!redirectable(bad), "{bad:?}");
        }
    }
}
