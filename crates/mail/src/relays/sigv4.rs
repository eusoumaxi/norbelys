//! AWS Signature Version 4: the signature every request to an AWS API carries, computed from the
//! request and the caller's secret access key
//! (<https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html>, read
//! 2026-10-02). Only the `Authorization` header form is produced, for the few `GET` requests a
//! connection's daily check makes to Amazon SES; the hashes and HMACs are `aws-lc-rs`'s.
//!
//! The signature covers a canonical form of the request:
//!
//! 1. **The canonical request**: the method; the path as it is sent, each segment URI-encoded
//!    once more (AWS signs the path encoded a second time for every service but Amazon S3, so an
//!    identity `ada@example.com`, sent as `ada%40example.com`, is signed as `ada%2540example.com`);
//!    the query parameters, each name and value URI-encoded, sorted; the signed headers (`host`,
//!    `x-amz-date` and any the caller adds), names lowercased, values trimmed with inner runs of
//!    spaces collapsed, sorted by name, one per line; their names joined by `;`; the payload's
//!    SHA-256 in lowercase hexadecimal.
//! 2. **The string to sign**: `AWS4-HMAC-SHA256`, the request's instant (`YYYYMMDDTHHMMSSZ`), the
//!    credential scope (`YYYYMMDD/<region>/<service>/aws4_request`) and the canonical request's
//!    SHA-256, one per line.
//! 3. **The signing key**: HMAC-SHA256 chained from `AWS4` followed by the secret, over the date,
//!    the Region, the service and `aws4_request`, so a key derived for one day, Region and
//!    service signs nothing else, and the secret itself never signs a request.
//!
//! URI encoding is AWS's own, not a form encoding: every byte but the unreserved
//! `A–Z a–z 0–9 - . _ ~` becomes `%XX` in uppercase hexadecimal, a space included (never `+`).
//!
//! The tests hold the signer to vectors of AWS's published Signature Version 4 test suite
//! (<https://github.com/awslabs/aws-c-auth/tree/main/tests/aws-signing-test-suite/v4>).

use std::fmt::Write as _;

use aws_lc_rs::{digest, hmac};
use jiff::Timestamp;
use secrecy::{ExposeSecret as _, SecretString};
use url::Url;

/// The algorithm's name: the first word of the `Authorization` header and of the string to sign.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// An AWS access key: what a request is signed with.
#[derive(Clone)]
pub struct AccessKey {
    /// The access key id (`AKIA…`), sent in the clear in the credential scope.
    pub id: String,
    /// The secret access key, which never leaves the process.
    pub secret: SecretString,
}

impl std::fmt::Debug for AccessKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What a request is signed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope<'a> {
    /// The Region (`us-east-1`).
    pub region: &'a str,
    /// The service's signing name (`ses`).
    pub service: &'a str,
}

/// The two headers a signed request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// `X-Amz-Date`: the request's instant, `YYYYMMDDTHHMMSSZ`.
    pub amz_date: String,
    /// `Authorization`: the algorithm, the credential scope, the signed headers' names and the
    /// signature.
    pub authorization: String,
}

/// Signs a request of `method` to `url` (its host, path and query as they are sent) carrying
/// `payload`, at the instant `at`. `headers` are signed besides `host` and `x-amz-date`; the
/// caller sends them exactly as given, with the two returned headers.
#[must_use]
pub fn sign(
    key: &AccessKey,
    scope: Scope<'_>,
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    payload: &[u8],
    at: Timestamp,
) -> Signed {
    let amz_date = at.strftime("%Y%m%dT%H%M%SZ").to_string();
    let date = at.strftime("%Y%m%d").to_string();
    let (canonical, signed_headers) = canonical_request(method, url, headers, &amz_date, payload);
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let signature =
        hex(hmac::sign(&signing_key(&key.secret, &date, scope), to_sign.as_bytes()).as_ref());
    Signed {
        authorization: format!(
            "{ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            key.id
        ),
        amz_date,
    }
}

/// AWS's URI encoding of `text` (see the module): what a path segment is sent as, and what the
/// canonical request encodes once more.
#[must_use]
pub fn uri_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// The canonical request of a request and its signed headers' names (see the module).
fn canonical_request(
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    amz_date: &str,
    payload: &[u8],
) -> (String, String) {
    let path = url
        .path()
        .split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/");
    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (uri_encode(&name), uri_encode(&value)))
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let mut signed: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), collapse(value)))
        .collect();
    signed.push(("host".to_owned(), host));
    signed.push(("x-amz-date".to_owned(), amz_date.to_owned()));
    signed.sort();
    let names = signed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical = format!("{method}\n{path}\n{query}\n");
    for (name, value) in &signed {
        let _ = writeln!(canonical, "{name}:{value}");
    }
    let _ = write!(canonical, "\n{names}\n{}", sha256_hex(payload));
    (canonical, names)
}

/// The key that signs the day's requests for `scope` (see the module).
fn signing_key(secret: &SecretString, date: &str, scope: Scope<'_>) -> hmac::Key {
    let step = |key: &[u8], data: &str| {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes())
    };
    let dated = step(format!("AWS4{}", secret.expose_secret()).as_bytes(), date);
    let regional = step(dated.as_ref(), scope.region);
    let service = step(regional.as_ref(), scope.service);
    let signing = step(service.as_ref(), "aws4_request");
    hmac::Key::new(hmac::HMAC_SHA256, signing.as_ref())
}

/// A header value as it is signed: trimmed, inner runs of whitespace collapsed to one space.
fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The SHA-256 of `bytes` in lowercase hexadecimal.
fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

/// `bytes` in lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use url::Url;

    use super::{AccessKey, Scope, canonical_request, sha256_hex, sign, uri_encode};

    /// The credentials, instant and scope of AWS's Signature Version 4 test suite.
    fn suite() -> (AccessKey, Scope<'static>, jiff::Timestamp) {
        (
            AccessKey {
                id: "AKIDEXAMPLE".to_owned(),
                secret: SecretString::from("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            },
            Scope {
                region: "us-east-1",
                service: "service",
            },
            "2015-08-30T12:36:00Z".parse().unwrap(),
        )
    }

    /// The suite's `get-vanilla` case, byte for byte: the canonical request of a bare `GET /`,
    /// its hash in the string to sign, and the signature AWS computes. A signer that drifts by
    /// one byte anywhere (a missing blank line, an unsorted header, the wrong key chain) fails
    /// here instead of at AWS with `SignatureDoesNotMatch`.
    #[test]
    fn signs_the_suites_vanilla_request() {
        let (key, scope, at) = suite();
        let url = Url::parse("https://example.amazonaws.com/").unwrap();
        let (canonical, names) = canonical_request("GET", &url, &[], "20150830T123600Z", b"");
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(names, "host;x-amz-date");
        assert_eq!(
            sha256_hex(canonical.as_bytes()),
            "bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        let signed = sign(&key, scope, "GET", &url, &[], b"", at);
        assert_eq!(signed.amz_date, "20150830T123600Z");
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// The suite's `get-vanilla-query-order-key-case` case: query parameters are signed sorted
    /// by name whatever order they are sent in, so the signature matches AWS's.
    #[test]
    fn signs_the_query_sorted() {
        let (key, scope, at) = suite();
        let url = Url::parse("https://example.amazonaws.com/?Param2=value2&Param1=value1").unwrap();
        let (canonical, _) = canonical_request("GET", &url, &[], "20150830T123600Z", b"");
        assert!(
            canonical.starts_with("GET\n/\nParam1=value1&Param2=value2\n"),
            "{canonical}"
        );
        assert!(
            sign(&key, scope, "GET", &url, &[], b"", at)
                .authorization
                .ends_with(
                    "Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
                )
        );
    }

    /// An SES address identity is sent with its `@` encoded and signed encoded twice, as AWS
    /// computes the path of every service but S3: signing it encoded once would make every
    /// identity read of an address fail its signature check.
    #[test]
    fn signs_a_path_encoded_twice() {
        assert_eq!(uri_encode("ada@example.com"), "ada%40example.com");
        assert_eq!(uri_encode("a b~c"), "a%20b~c");
        let url = Url::parse(&format!(
            "https://email.us-east-1.amazonaws.com/v2/email/identities/{}",
            uri_encode("ada@example.com")
        ))
        .unwrap();
        assert_eq!(url.path(), "/v2/email/identities/ada%40example.com");
        let (canonical, _) = canonical_request("GET", &url, &[], "20260102T030405Z", b"");
        assert!(
            canonical.starts_with("GET\n/v2/email/identities/ada%2540example.com\n\n"),
            "{canonical}"
        );
    }
}
