//! DKIM signature verification (RFC 6376, <https://www.rfc-editor.org/rfc/rfc6376>): whether a
//! message carries a valid signature of a domain. A passing signature proves that the holder of
//! the domain's key wrote the header fields it covers and the body, which is how a feedback-loop
//! report proves who sent it, and how a message returned inside a report proves it was ours.
//!
//! # What is verified
//!
//! Each `DKIM-Signature` header field, from the top, at most [`MAX_SIGNATURES`] of them (each may
//! cost a DNS lookup, so no message can make the verifier ask DNS without bound):
//!
//! 1. Its tags (RFC 6376 §3.5): `v=1`, `a=`, `b=`, `bh=`, `d=`, `h=` (which must name `From`) and
//!    `s=` are required; `i=`, when present, is `d=` or a subdomain of it; `q=`, when present,
//!    offers `dns/txt`; `x=`, when present, has not passed. A tag given twice makes the whole
//!    signature malformed.
//! 2. The algorithm: `rsa-sha256` (RFC 6376 §3.3.1) with keys of 1,024 to 8,192 bits, the range
//!    RFC 8301 requires verifiers to accept, or `ed25519-sha256` (RFC 8463,
//!    <https://www.rfc-editor.org/rfc/rfc8463>), whose Ed25519 signature is over the SHA-256
//!    hash of the signed data rather than the data itself. `rsa-sha1` is refused, as RFC 8301
//!    requires, and so is any other algorithm.
//! 3. Canonicalization (RFC 6376 §3.4): `simple` or `relaxed`, for the header and for the body,
//!    as `c=` says (`simple/simple` when absent).
//! 4. The body hash `bh=`, over the whole canonical body. A body length limit (`l=`) is refused:
//!    it lets anyone append content after the signed part, and the reports this verifier serves
//!    are read whole.
//! 5. The key: the TXT record at `<s>._domainkey.<d>` (RFC 6376 §3.6.2), from a [`KeySource`]:
//!    the process's DNS resolver, or records the caller already holds. Its `k=` must match the
//!    algorithm, its `h=` (when present) must allow SHA-256 and its `s=` (when present) email; an
//!    empty `p=` means the key was revoked; with `t=s`, `i=` must be `d=` itself.
//! 6. The signature `b=`, over the header fields `h=` names, each taken from the bottom of the
//!    header block up (a name listed more often than its field occurs contributes nothing, which
//!    is how a signer keeps a field from being added later), followed by the signature's own
//!    field with the value of `b=` deleted, canonicalized, without its final CRLF.
//!
//! Bare LF line ends are read as CRLF, the form the signer hashed.
//!
//! [`verify_headers`] checks a header block alone and skips `bh=`: what a feedback report returns
//! as `text/rfc822-headers` has no body, yet a passing signature still proves every field it
//! covers.
//!
//! Each signature gets its own [`Verdict`]; what a passing domain means (an enrolled feedback
//! loop, one of our own domains) is the caller's decision.

use std::borrow::Cow;
use std::collections::HashMap;

use aws_lc_rs::digest;
use aws_lc_rs::signature::{
    ED25519, RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, UnparsedPublicKey,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hickory_resolver::proto::rr::RData;
use jiff::Timestamp;

/// The most signatures of one message that are verified; any below them are ignored.
pub const MAX_SIGNATURES: usize = 8;

/// The name of the header field that carries a signature, lowercase.
const FIELD: &str = "dkim-signature";

/// Where public keys come from: the TXT records at a key's name. Two sources exist: the DNS
/// resolver of the process (any domain's key), and a set of records the caller already holds
/// (the managed MTA's own keys, which it never needs to look up).
pub trait KeySource {
    /// The TXT records at `name` (`<selector>._domainkey.<domain>`, lowercase, without the
    /// trailing dot), each record's strings joined; empty when the name has none.
    ///
    /// # Errors
    ///
    /// The records could not be read now (a timeout, a server failure): worth trying again.
    fn txt(&self, name: &str) -> impl Future<Output = Result<Vec<String>, KeyUnavailable>> + Send;
}

/// Why a key's records could not be read now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct KeyUnavailable(pub String);

impl KeySource for hickory_resolver::TokioResolver {
    /// Asks DNS, fully qualified: a name with no TXT record, or no such name, answers no
    /// records; any other failure is temporary.
    fn txt(&self, name: &str) -> impl Future<Output = Result<Vec<String>, KeyUnavailable>> + Send {
        let fqdn = format!("{}.", name.trim_end_matches('.'));
        async move {
            match self.txt_lookup(fqdn.as_str()).await {
                Ok(lookup) => Ok(lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        RData::TXT(txt) => {
                            Some(String::from_utf8_lossy(&txt.txt_data.concat()).into_owned())
                        }
                        _ => None,
                    })
                    .collect()),
                Err(error) if error.is_no_records_found() || error.is_nx_domain() => Ok(Vec::new()),
                Err(error) => Err(KeyUnavailable(format!(
                    "the TXT lookup of {fqdn} failed: {error}"
                ))),
            }
        }
    }
}

impl KeySource for HashMap<String, String> {
    /// The record held under `name` (lowercase, without the trailing dot), if any.
    fn txt(&self, name: &str) -> impl Future<Output = Result<Vec<String>, KeyUnavailable>> + Send {
        let found: Vec<String> = self
            .get(&name.trim_end_matches('.').to_ascii_lowercase())
            .cloned()
            .into_iter()
            .collect();
        std::future::ready(Ok::<_, KeyUnavailable>(found))
    }
}

/// What one signature's verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The signing domain (`d=`), lowercase; empty when the signature does not name one.
    pub domain: String,
    /// The selector (`s=`), lowercase; empty when the signature does not name one.
    pub selector: String,
    /// The header fields the signature covers (`h=`), lowercase, in order, repeats kept.
    pub signed_headers: Vec<String>,
    /// `Ok` when the signature verifies.
    pub result: Result<(), Failure>,
}

impl Verdict {
    /// Whether the signature verifies and covers the header field `name` (any case).
    #[must_use]
    pub fn covers(&self, name: &str) -> bool {
        self.result.is_ok()
            && self
                .signed_headers
                .iter()
                .any(|signed| signed.eq_ignore_ascii_case(name))
    }
}

/// Why a signature does not verify.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// The signature breaks RFC 6376's syntax or rules: a required tag missing, a tag twice,
    /// `i=` outside `d=`, `h=` without `From`, a value that is not base64.
    #[error("malformed signature: {0}")]
    Malformed(String),
    /// An algorithm, canonicalization or query method this verifier does not accept, or a body
    /// length limit.
    #[error("unsupported signature: {0}")]
    Unsupported(String),
    /// The signature's expiry (`x=`) has passed.
    #[error("the signature expired")]
    Expired,
    /// The key's records could not be read now; verifying again later may succeed.
    #[error("the key could not be read: {0}")]
    KeyUnavailable(String),
    /// No usable key: none published, revoked, of another type, or not for email.
    #[error("no usable key: {0}")]
    KeyInvalid(String),
    /// The body does not hash to `bh=`: it changed after it was signed.
    #[error("the body hash does not match")]
    BodyHashMismatch,
    /// The signature does not verify with the key: a signed field changed, or it is forged.
    #[error("the signature does not verify")]
    SignatureMismatch,
}

/// A public key record (RFC 6376 §3.6.1), as published at `<selector>._domainkey.<domain>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    /// `k=`, lowercase: `rsa` (the default) or `ed25519`.
    pub kind: String,
    /// `p=`, decoded: an RSA key as DER (SubjectPublicKeyInfo or RSAPublicKey), or the 32 bytes
    /// of an Ed25519 key; empty when the key was revoked.
    pub key: Vec<u8>,
    /// `t=s`: a signature's `i=` domain must be its `d=` exactly.
    pub strict: bool,
}

impl KeyRecord {
    /// The key record in `text`.
    ///
    /// # Errors
    ///
    /// It is not a tag list, its version is not `DKIM1`, it allows no SHA-256 or no email, or
    /// its `p=` is missing or not base64.
    pub fn parse(text: &str) -> Result<Self, Failure> {
        let invalid = |detail: &str| Failure::KeyInvalid(detail.to_owned());
        let tags = tags(text).ok_or_else(|| invalid("the key record is not a tag list"))?;
        let get = |name: &str| tag(&tags, name);
        if get("v").is_some_and(|version| version != "DKIM1") {
            return Err(invalid("the key record's version is not DKIM1"));
        }
        if get("h").is_some_and(|hashes| {
            !hashes
                .split(':')
                .any(|hash| hash.trim().eq_ignore_ascii_case("sha256"))
        }) {
            return Err(invalid("the key does not allow SHA-256"));
        }
        if get("s").is_some_and(|services| {
            !services
                .split(':')
                .any(|service| matches!(service.trim(), "*" | "email"))
        }) {
            return Err(invalid("the key is not for email"));
        }
        let encoded: String = get("p")
            .ok_or_else(|| invalid("the key record has no p="))?
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let key = if encoded.is_empty() {
            Vec::new()
        } else {
            STANDARD
                .decode(encoded)
                .map_err(|_| invalid("the key's p= is not base64"))?
        };
        Ok(Self {
            kind: get("k").unwrap_or("rsa").to_ascii_lowercase(),
            key,
            strict: get("t").is_some_and(|flags| flags.split(':').any(|flag| flag.trim() == "s")),
        })
    }
}

/// Verifies every signature of `message` (see the module), reading keys from `keys`, at `now`.
pub async fn verify<K: KeySource + Sync>(message: &[u8], keys: &K, now: Timestamp) -> Vec<Verdict> {
    run(message, keys, now, true).await
}

/// Verifies every signature of a header block that comes without its body (the returned
/// headers of a feedback report): as [`verify`], but `bh=` is not checked.
pub async fn verify_headers<K: KeySource + Sync>(
    headers: &[u8],
    keys: &K,
    now: Timestamp,
) -> Vec<Verdict> {
    run(headers, keys, now, false).await
}

/// The signing domain and selector of each signature of `message` that names both, lowercase,
/// in order: the keys a caller who holds its own records must supply.
#[must_use]
pub fn signers(message: &[u8]) -> Vec<(String, String)> {
    let message = crlf(message);
    let (fields, _) = split(&message);
    fields
        .iter()
        .filter(|field| field.name == FIELD)
        .take(MAX_SIGNATURES)
        .filter_map(|field| {
            let tags = tags(&String::from_utf8_lossy(field.value()))?;
            Some((
                tag(&tags, "d")?.to_ascii_lowercase(),
                tag(&tags, "s")?.to_ascii_lowercase(),
            ))
        })
        .collect()
}

/// One header field of a message.
struct Field<'a> {
    /// Its name, lowercase, without surrounding whitespace.
    name: String,
    /// Where it starts in the message.
    start: usize,
    /// Its bytes as received: the first line, its continuation lines, the line ends.
    raw: &'a [u8],
}

impl Field<'_> {
    /// Everything after the first colon.
    fn value(&self) -> &[u8] {
        self.raw
            .iter()
            .position(|&byte| byte == b':')
            .and_then(|colon| self.raw.get(colon + 1..))
            .unwrap_or_default()
    }
}

/// How a part of the message is canonicalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Canon {
    Simple,
    Relaxed,
}

/// A signing algorithm this verifier accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    RsaSha256,
    Ed25519Sha256,
}

impl Algorithm {
    /// The `k=` of the keys it verifies with.
    fn key_kind(self) -> &'static str {
        match self {
            Self::RsaSha256 => "rsa",
            Self::Ed25519Sha256 => "ed25519",
        }
    }
}

async fn run<K: KeySource + Sync>(
    message: &[u8],
    keys: &K,
    now: Timestamp,
    check_body: bool,
) -> Vec<Verdict> {
    let message = crlf(message);
    let (fields, body) = split(&message);
    let mut verdicts = Vec::new();
    for (index, field) in fields
        .iter()
        .enumerate()
        .filter(|(_, field)| field.name == FIELD)
        .take(MAX_SIGNATURES)
    {
        let value = String::from_utf8_lossy(field.value()).into_owned();
        let Some(tags) = tags(&value) else {
            verdicts.push(Verdict {
                domain: String::new(),
                selector: String::new(),
                signed_headers: Vec::new(),
                result: Err(Failure::Malformed(
                    "the tag list repeats a tag or has a tag without `=`".to_owned(),
                )),
            });
            continue;
        };
        let domain = tag(&tags, "d").unwrap_or_default().to_ascii_lowercase();
        let selector = tag(&tags, "s").unwrap_or_default().to_ascii_lowercase();
        let signed_headers: Vec<String> = tag(&tags, "h")
            .unwrap_or_default()
            .split(':')
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect();
        let signature = Signed {
            fields: &fields,
            index,
            body,
            tags: &tags,
            domain: &domain,
            selector: &selector,
            signed_headers: &signed_headers,
        };
        let result = signature.check(keys, now, check_body).await;
        verdicts.push(Verdict {
            domain,
            selector,
            signed_headers,
            result,
        });
    }
    verdicts
}

/// One signature in its message, its tags read.
struct Signed<'a> {
    fields: &'a [Field<'a>],
    /// The signature's own field among `fields`.
    index: usize,
    body: &'a [u8],
    tags: &'a [(String, String)],
    domain: &'a str,
    selector: &'a str,
    signed_headers: &'a [String],
}

impl Signed<'_> {
    /// The verification itself (see the module), cheapest checks first: the key is looked up
    /// only for a well-formed signature whose body hash matches.
    async fn check<K: KeySource + Sync>(
        &self,
        keys: &K,
        now: Timestamp,
        check_body: bool,
    ) -> Result<(), Failure> {
        let malformed = |detail: &str| Failure::Malformed(detail.to_owned());
        let get = |name: &str| tag(self.tags, name);
        if get("v") != Some("1") {
            return Err(malformed("v= is not 1"));
        }
        let algorithm = match get("a").unwrap_or_default().to_ascii_lowercase().as_str() {
            "rsa-sha256" => Algorithm::RsaSha256,
            "ed25519-sha256" => Algorithm::Ed25519Sha256,
            "" => return Err(malformed("a= is missing")),
            other => {
                return Err(Failure::Unsupported(format!(
                    "the algorithm {}",
                    crate::text::bounded(other, 64)
                )));
            }
        };
        let (header_canon, body_canon) = canonicalization(get("c"))?;
        if self.domain.is_empty() || self.selector.is_empty() {
            return Err(malformed("d= and s= are required"));
        }
        if !self.signed_headers.iter().any(|name| name == "from") {
            return Err(malformed("h= does not name From"));
        }
        if get("l").is_some() {
            return Err(Failure::Unsupported("a body length limit (l=)".to_owned()));
        }
        if get("q").is_some_and(|methods| {
            !methods
                .split(':')
                .any(|method| method.trim().eq_ignore_ascii_case("dns/txt"))
        }) {
            return Err(Failure::Unsupported(
                "a query method other than dns/txt".to_owned(),
            ));
        }
        let identity = match get("i") {
            Some(identity) => {
                let domain = identity
                    .rsplit_once('@')
                    .map(|(_, domain)| domain.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                if !within(&domain, self.domain) {
                    return Err(malformed("i= is outside d="));
                }
                Some(domain)
            }
            None => None,
        };
        if let Some(expires) = get("x") {
            let expires: i64 = expires
                .parse()
                .map_err(|_| malformed("x= is not a number"))?;
            if expires < now.as_second() {
                return Err(Failure::Expired);
            }
        }
        let body_hash = base64(get("bh")).ok_or_else(|| malformed("bh= is not base64"))?;
        let signature = base64(get("b")).ok_or_else(|| malformed("b= is not base64"))?;
        if check_body {
            let canonical = match body_canon {
                Canon::Simple => simple_body(self.body),
                Canon::Relaxed => relaxed_body(self.body),
            };
            if digest::digest(&digest::SHA256, &canonical).as_ref() != body_hash.as_slice() {
                return Err(Failure::BodyHashMismatch);
            }
        }
        let key = key(keys, self.domain, self.selector, algorithm).await?;
        if key.strict
            && identity
                .as_deref()
                .is_some_and(|identity| identity != self.domain)
        {
            return Err(malformed("the key's t=s requires i= to be d= itself"));
        }
        let own = self
            .fields
            .get(self.index)
            .map(|field| field.raw)
            .unwrap_or_default();
        let data = signed_data(
            self.fields,
            Some(self.index),
            own,
            self.signed_headers,
            header_canon,
        );
        if verifies(algorithm, &key.key, &data, &signature) {
            Ok(())
        } else {
            Err(Failure::SignatureMismatch)
        }
    }
}

/// Whether `signature` signs `data` under `key` with `algorithm`. Ed25519 signs the SHA-256 hash
/// of the data (RFC 8463 §3); RSA signs the data with PKCS#1 v1.5 padding over SHA-256.
fn verifies(algorithm: Algorithm, key: &[u8], data: &[u8], signature: &[u8]) -> bool {
    match algorithm {
        Algorithm::RsaSha256 => {
            UnparsedPublicKey::new(&RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, key)
                .verify(data, signature)
                .is_ok()
        }
        Algorithm::Ed25519Sha256 => {
            let hash = digest::digest(&digest::SHA256, data);
            UnparsedPublicKey::new(&ED25519, key)
                .verify(hash.as_ref(), signature)
                .is_ok()
        }
    }
}

/// The key of `domain` and `selector` for `algorithm`: the first record at its name that parses
/// with the algorithm's key type and is not revoked.
async fn key<K: KeySource + Sync>(
    keys: &K,
    domain: &str,
    selector: &str,
    algorithm: Algorithm,
) -> Result<KeyRecord, Failure> {
    let name = format!("{selector}._domainkey.{domain}");
    let records = keys
        .txt(&name)
        .await
        .map_err(|error| Failure::KeyUnavailable(error.0))?;
    let mut refused = None;
    for text in &records {
        match KeyRecord::parse(text) {
            Ok(record) if record.kind != algorithm.key_kind() => {
                refused = Some(Failure::KeyInvalid(format!(
                    "the key at {name} is {}, not {}",
                    crate::text::bounded(&record.kind, 32),
                    algorithm.key_kind()
                )));
            }
            Ok(record) if record.key.is_empty() => {
                refused = Some(Failure::KeyInvalid(format!(
                    "the key at {name} was revoked"
                )));
            }
            Ok(record) => return Ok(record),
            Err(error) => refused = Some(error),
        }
    }
    Err(refused.unwrap_or_else(|| Failure::KeyInvalid(format!("no key record at {name}"))))
}

/// The data a signature signs: the fields `names` lists, each the lowest one of its name not
/// yet taken (the signature's own field, at `own_index`, is never taken), canonicalized, then
/// `own` (the signature's field) with the value of `b=` deleted, canonicalized, without its
/// final CRLF.
fn signed_data(
    fields: &[Field<'_>],
    own_index: Option<usize>,
    own: &[u8],
    names: &[String],
    canon: Canon,
) -> Vec<u8> {
    let mut taken: Vec<bool> = (0..fields.len())
        .map(|index| Some(index) == own_index)
        .collect();
    let mut data = Vec::new();
    for name in names {
        let found = fields
            .iter()
            .enumerate()
            .rev()
            .find(|(index, field)| {
                field.name == *name && !taken.get(*index).copied().unwrap_or(true)
            })
            .map(|(index, field)| (index, field.raw));
        if let Some((index, raw)) = found {
            if let Some(flag) = taken.get_mut(index) {
                *flag = true;
            }
            data.extend_from_slice(&canonical_header(name, raw, canon));
        }
    }
    let mut signature = canonical_header(FIELD, &without_b(own), canon);
    if signature.ends_with(b"\r\n") {
        signature.truncate(signature.len().saturating_sub(2));
    }
    data.extend_from_slice(&signature);
    data
}

/// A header field canonicalized (RFC 6376 §3.4.1 and §3.4.2). `simple` keeps it as it is, with
/// its line end. `relaxed` writes the name (`name`, already lowercase) and a colon, then the
/// value unfolded, each run of spaces and tabs as one space, none at its start or end, then CRLF.
fn canonical_header(name: &str, raw: &[u8], canon: Canon) -> Vec<u8> {
    match canon {
        Canon::Simple => {
            let mut out = raw.to_vec();
            if !out.ends_with(b"\r\n") {
                out.extend_from_slice(b"\r\n");
            }
            out
        }
        Canon::Relaxed => {
            let value = raw
                .iter()
                .position(|&byte| byte == b':')
                .and_then(|colon| raw.get(colon + 1..))
                .unwrap_or_default();
            let mut out = Vec::with_capacity(raw.len());
            out.extend_from_slice(name.as_bytes());
            out.push(b':');
            let (mut started, mut space) = (false, false);
            for &byte in value {
                match byte {
                    b'\r' | b'\n' => {}
                    b' ' | b'\t' => space = started,
                    other => {
                        if space {
                            out.push(b' ');
                        }
                        space = false;
                        started = true;
                        out.push(other);
                    }
                }
            }
            out.extend_from_slice(b"\r\n");
            out
        }
    }
}

/// The signature field `raw` with the value of its `b=` tag deleted, the whitespace around that
/// value included, and every other byte kept (RFC 6376 §3.7).
fn without_b(raw: &[u8]) -> Vec<u8> {
    let Some(colon) = raw.iter().position(|&byte| byte == b':') else {
        return raw.to_vec();
    };
    let mut out = raw.get(..=colon).unwrap_or_default().to_vec();
    for (index, segment) in raw
        .get(colon + 1..)
        .unwrap_or_default()
        .split(|&byte| byte == b';')
        .enumerate()
    {
        if index > 0 {
            out.push(b';');
        }
        let equals = segment.iter().position(|&byte| byte == b'=');
        match equals {
            Some(equals)
                if segment
                    .get(..equals)
                    .is_some_and(|name| name.trim_ascii() == b"b") =>
            {
                out.extend_from_slice(segment.get(..=equals).unwrap_or_default());
            }
            _ => out.extend_from_slice(segment),
        }
    }
    out
}

/// The body canonicalized by `simple` (RFC 6376 §3.4.3): empty lines at its end removed, then
/// exactly one CRLF at its end (an empty body is one CRLF).
fn simple_body(body: &[u8]) -> Vec<u8> {
    let mut end = body.len();
    while body
        .get(..end)
        .is_some_and(|kept| kept.ends_with(b"\r\n\r\n"))
    {
        end = end.saturating_sub(2);
    }
    let mut out = body.get(..end).unwrap_or_default().to_vec();
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The body canonicalized by `relaxed` (RFC 6376 §3.4.4): in each line, spaces and tabs at its
/// end removed and every other run of them made one space; empty lines at the end removed; each
/// line ended by CRLF (an empty body stays empty).
fn relaxed_body(body: &[u8]) -> Vec<u8> {
    let mut segments: Vec<&[u8]> = body.split(|&byte| byte == b'\n').collect();
    if body.is_empty() || body.ends_with(b"\n") {
        segments.pop();
    }
    let mut lines: Vec<Vec<u8>> = segments
        .into_iter()
        .map(|segment| {
            let segment = segment.strip_suffix(b"\r").unwrap_or(segment);
            let mut line = Vec::with_capacity(segment.len());
            let mut space = false;
            for &byte in segment {
                if byte == b' ' || byte == b'\t' {
                    space = true;
                } else {
                    if space {
                        line.push(b' ');
                    }
                    space = false;
                    line.push(byte);
                }
            }
            line
        })
        .collect();
    while lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    let mut out = Vec::with_capacity(body.len());
    for line in lines {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// The header and body canonicalizations of `c=` (`simple/simple` when absent; a missing body
/// part is `simple`).
fn canonicalization(value: Option<&str>) -> Result<(Canon, Canon), Failure> {
    let parse = |name: &str| match name.trim().to_ascii_lowercase().as_str() {
        "simple" => Ok(Canon::Simple),
        "relaxed" => Ok(Canon::Relaxed),
        other => Err(Failure::Unsupported(format!(
            "the canonicalization {}",
            crate::text::bounded(other, 32)
        ))),
    };
    match value {
        None => Ok((Canon::Simple, Canon::Simple)),
        Some(value) => match value.split_once('/') {
            Some((header, body)) => Ok((parse(header)?, parse(body)?)),
            None => Ok((parse(value)?, Canon::Simple)),
        },
    }
}

/// `child` is `parent` or one of its subdomains.
fn within(child: &str, parent: &str) -> bool {
    child == parent
        || child
            .strip_suffix(parent)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// A base64 tag value, whitespace (folding) removed; `None` when absent, empty or not base64.
fn base64(value: Option<&str>) -> Option<Vec<u8>> {
    let compact: String = value?.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }
    STANDARD.decode(compact).ok()
}

/// A tag list (RFC 6376 §3.2): `name=value` pairs separated by `;`, whitespace around names and
/// values ignored, a final `;` allowed; `None` when a pair has no `=`, a name is empty, or a
/// name repeats.
fn tags(text: &str) -> Option<Vec<(String, String)>> {
    let mut tags: Vec<(String, String)> = Vec::new();
    for pair in text.split(';') {
        if pair.trim().is_empty() {
            continue;
        }
        let (name, value) = pair.split_once('=')?;
        let name = name.trim();
        if name.is_empty() || tags.iter().any(|(seen, _)| seen == name) {
            return None;
        }
        tags.push((name.to_owned(), value.trim().to_owned()));
    }
    Some(tags)
}

/// The value of the tag `name` (tag names are case-sensitive).
fn tag<'a>(tags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(tag, _)| tag == name)
        .map(|(_, value)| value.as_str())
}

/// `message` with every bare LF made CRLF; borrowed when there is none.
fn crlf(message: &[u8]) -> Cow<'_, [u8]> {
    let bare = message.iter().enumerate().any(|(at, &byte)| {
        byte == b'\n' && at.checked_sub(1).and_then(|before| message.get(before)) != Some(&b'\r')
    });
    if !bare {
        return Cow::Borrowed(message);
    }
    let mut out = Vec::with_capacity(message.len().saturating_add(message.len() / 16));
    let mut previous = 0_u8;
    for &byte in message {
        if byte == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        previous = byte;
    }
    Cow::Owned(out)
}

/// The header fields of `message` (CRLF line ends) and its body: the fields end at the first
/// empty line, the body is everything after it (empty when there is no empty line). A line
/// starting with a space or a tab continues the field above it; a line without a colon is not a
/// field and is skipped.
fn split(message: &[u8]) -> (Vec<Field<'_>>, &[u8]) {
    let mut fields: Vec<Field<'_>> = Vec::new();
    let mut start = 0;
    while let Some(rest) = message.get(start..).filter(|rest| !rest.is_empty()) {
        let end = rest
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(message.len(), |at| start + at + 1);
        let line = message.get(start..end).unwrap_or_default();
        if line == b"\r\n" {
            return (fields, message.get(end..).unwrap_or_default());
        }
        if matches!(line.first(), Some(b' ' | b'\t')) {
            if let Some(field) = fields.last_mut() {
                field.raw = message.get(field.start..end).unwrap_or_default();
            }
        } else if let Some(colon) = line.iter().position(|&byte| byte == b':') {
            fields.push(Field {
                name: String::from_utf8_lossy(line.get(..colon).unwrap_or_default())
                    .trim()
                    .to_ascii_lowercase(),
                start,
                raw: line,
            });
        }
        start = end;
    }
    (fields, &[])
}

/// A private key that signs: for the tests of code that verifies, here and in the crates that
/// call this one.
#[cfg(any(test, feature = "test-support"))]
pub enum SigningKey {
    /// Signs `ed25519-sha256`.
    Ed25519(aws_lc_rs::signature::Ed25519KeyPair),
    /// Signs `rsa-sha256`.
    Rsa(aws_lc_rs::rsa::KeyPair),
}

/// `message` with a `DKIM-Signature` of `domain` and `selector` prepended, `relaxed/relaxed`,
/// over the header fields `headers` names: what a signer sends, for tests.
///
/// # Errors
///
/// The RSA signature could not be made.
#[cfg(any(test, feature = "test-support"))]
pub fn sign(
    message: &[u8],
    key: &SigningKey,
    domain: &str,
    selector: &str,
    headers: &[&str],
) -> Result<Vec<u8>, aws_lc_rs::error::Unspecified> {
    let message = crlf(message);
    let (fields, body) = split(&message);
    let body_hash = STANDARD.encode(digest::digest(&digest::SHA256, &relaxed_body(body)));
    let algorithm = match key {
        SigningKey::Ed25519(_) => "ed25519-sha256",
        SigningKey::Rsa(_) => "rsa-sha256",
    };
    let field = format!(
        "DKIM-Signature: v=1; a={algorithm}; c=relaxed/relaxed; d={domain}; s={selector}; h={}; bh={body_hash}; b=",
        headers.join(":")
    );
    let names: Vec<String> = headers
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let data = signed_data(&fields, None, field.as_bytes(), &names, Canon::Relaxed);
    let signature = match key {
        SigningKey::Ed25519(pair) => pair
            .sign(digest::digest(&digest::SHA256, &data).as_ref())
            .as_ref()
            .to_vec(),
        SigningKey::Rsa(pair) => {
            let mut signature = vec![0; pair.public_modulus_len()];
            pair.sign(
                &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                &aws_lc_rs::rand::SystemRandom::new(),
                &data,
                &mut signature,
            )?;
            signature
        }
    };
    let mut signed = format!("{field}{}\r\n", STANDARD.encode(signature)).into_bytes();
    signed.extend_from_slice(&message);
    Ok(signed)
}

#[cfg(test)]
mod tests;
