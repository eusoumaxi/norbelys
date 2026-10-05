use std::collections::HashMap;

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rsa::{KeyPair as RsaKeyPair, KeySize};
use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;

use super::{
    Canon, Failure, KeyRecord, KeySource, KeyUnavailable, SigningKey, canonical_header, crlf,
    relaxed_body, sign, signed_data, signers, simple_body, split, verify, verify_headers,
};

/// RFC 8463, Appendix A.3: one message signed by `football.example.com` twice, with Ed25519
/// (selector `brisbane`) and with RSA (selector `test`), byte for byte as published (each line
/// at the first position, continuation lines with one space, a blank line after "Joe.").
const SIGNED: &str = concat!(
    "DKIM-Signature: v=1; a=ed25519-sha256; c=relaxed/relaxed;\r\n",
    " d=football.example.com; i=@football.example.com;\r\n",
    " q=dns/txt; s=brisbane; t=1528637909; h=from : to :\r\n",
    " subject : date : message-id : from : subject : date;\r\n",
    " bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n",
    " b=/gCrinpcQOoIfuHNQIbq4pgh9kyIK3AQUdt9OdqQehSwhEIug4D11Bus\r\n",
    " Fa3bT3FY5OsU7ZbnKELq+eXdp1Q1Dw==\r\n",
    "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;\r\n",
    " d=football.example.com; i=@football.example.com;\r\n",
    " q=dns/txt; s=test; t=1528637909; h=from : to : subject :\r\n",
    " date : message-id : from : subject : date;\r\n",
    " bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n",
    " b=F45dVWDfMbQDGHJFlXUNB2HKfbCeLRyhDXgFpEL8GwpsRe0IeIixNTe3\r\n",
    " DhCVlUrSjV4BwcVcOF6+FF3Zo9Rpo1tFOeS9mPYQTnGdaSGsgeefOsk2Jz\r\n",
    " dA+L10TeYt9BgDfQNZtKdN1WO//KgIqXP7OdEFE4LjFYNcUxZQ4FADY+8=\r\n",
    "From: Joe SixPack <joe@football.example.com>\r\n",
    "To: Suzie Q <suzie@shopping.example.net>\r\n",
    "Subject: Is dinner ready?\r\n",
    "Date: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\n",
    "Message-ID: <20030712040037.46341.5F8J@football.example.com>\r\n",
    "\r\n",
    "Hi.\r\n",
    "\r\n",
    "We lost the game.  Are you hungry yet?\r\n",
    "\r\n",
    "Joe.\r\n",
    "\r\n",
);

/// RFC 8463, Appendix A.2: the key records, each record's strings joined.
const ED25519_RECORD: &str = "v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
const RSA_RECORD: &str = concat!(
    "v=DKIM1; k=rsa; p=MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDkHlOQoBTzWR",
    "iGs5V6NpP3idY6Wk08a5qhdR6wy5bdOKb2jLQiY/J16JYi0Qvx/byYzCNb3W91y3FutAC",
    "DfzwQ/BC/e/8uBsCR+yz1Lxj+PL6lHvqMKrM3rG4hstT5QjvHO9PzoxZyVYLzBfO2EeC3",
    "Ip3G+2kryOTIKT+l/K4w3QIDAQAB",
);

/// The RFC's keys, as a resolver would answer them.
fn rfc_keys() -> HashMap<String, String> {
    HashMap::from([
        (
            "brisbane._domainkey.football.example.com".to_owned(),
            ED25519_RECORD.to_owned(),
        ),
        (
            "test._domainkey.football.example.com".to_owned(),
            RSA_RECORD.to_owned(),
        ),
    ])
}

/// 2026-10-02T03:00:00Z.
fn now() -> Timestamp {
    Timestamp::from_second(1_790_910_000).unwrap()
}

/// The outcome of each signature of `message`, in order.
async fn results(message: &str, keys: &HashMap<String, String>) -> Vec<Result<(), Failure>> {
    verify(message.as_bytes(), keys, now())
        .await
        .into_iter()
        .map(|verdict| verdict.result)
        .collect()
}

/// A key source whose every lookup fails, as a resolver does when DNS times out.
struct Down;

impl KeySource for Down {
    fn txt(&self, _: &str) -> impl Future<Output = Result<Vec<String>, KeyUnavailable>> + Send {
        std::future::ready(Err::<Vec<String>, _>(KeyUnavailable(
            "timed out".to_owned(),
        )))
    }
}

/// The published example of RFC 8463 verifies under both of its signatures with the published
/// keys: the vectors pin canonicalization, the signed data, the 1,024-bit RSA key RFC 8301 still
/// requires verifiers to accept, and Ed25519 over the SHA-256 hash, independently of any signer
/// written here.
#[tokio::test]
async fn the_rfc_8463_example_verifies() {
    let verdicts = verify(SIGNED.as_bytes(), &rfc_keys(), now()).await;
    let summary: Vec<(&str, &str, Result<(), Failure>)> = verdicts
        .iter()
        .map(|verdict| {
            (
                verdict.domain.as_str(),
                verdict.selector.as_str(),
                verdict.result.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("football.example.com", "brisbane", Ok(())),
            ("football.example.com", "test", Ok(())),
        ]
    );
    assert!(verdicts[0].covers("Message-ID"));
    assert!(!verdicts[0].covers("Feedback-ID"));
}

/// What the Ed25519 signature of RFC 8463 signs, byte for byte: the relaxed fields `h=` names,
/// each taken from the bottom up (From, Subject and Date are listed twice and the second time
/// contributes nothing), then the signature's own field with `b=` emptied and no final CRLF.
#[test]
fn the_signed_data_is_the_relaxed_header() {
    let message = crlf(SIGNED.as_bytes());
    let (fields, _) = split(&message);
    let names: Vec<String> = [
        "from",
        "to",
        "subject",
        "date",
        "message-id",
        "from",
        "subject",
        "date",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let data = signed_data(&fields, Some(0), fields[0].raw, &names, Canon::Relaxed);
    assert_eq!(
        String::from_utf8(data).unwrap(),
        concat!(
            "from:Joe SixPack <joe@football.example.com>\r\n",
            "to:Suzie Q <suzie@shopping.example.net>\r\n",
            "subject:Is dinner ready?\r\n",
            "date:Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\n",
            "message-id:<20030712040037.46341.5F8J@football.example.com>\r\n",
            "dkim-signature:v=1; a=ed25519-sha256; c=relaxed/relaxed; d=football.example.com; ",
            "i=@football.example.com; q=dns/txt; s=brisbane; t=1528637909; h=from : to : subject : ",
            "date : message-id : from : subject : date; bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=; b=",
        )
    );
}

/// RFC 6376's own canonicalization example (§3.4.5): relaxed lowercases names, unfolds, makes
/// each run of whitespace one space and drops it at the ends; simple keeps the header as it is;
/// both drop the empty lines that end a body, and an empty body is nothing (relaxed) or one CRLF
/// (simple).
#[test]
fn canonicalizes_the_rfc_6376_example() {
    let message: &[u8] = b"A: X\r\nB : Y\t\r\n\tZ  \r\n\r\n C \r\nD \t E\r\n\r\n\r\n";
    let (fields, body) = split(message);
    let header = |canon| -> Vec<u8> {
        fields
            .iter()
            .flat_map(|field| canonical_header(&field.name, field.raw, canon))
            .collect()
    };
    assert_eq!(header(Canon::Relaxed), b"a:X\r\nb:Y Z\r\n");
    assert_eq!(header(Canon::Simple), b"A: X\r\nB : Y\t\r\n\tZ  \r\n");
    assert_eq!(relaxed_body(body), b" C\r\nD E\r\n");
    assert_eq!(simple_body(body), b" C \r\nD \t E\r\n");
    assert_eq!(relaxed_body(b""), b"");
    assert_eq!(simple_body(b""), b"\r\n");
    assert_eq!(simple_body(b"\r\n\r\n"), b"\r\n");
}

/// A message changed after it was signed fails: a changed body fails the body hash under both
/// algorithms; a changed signed field fails the signature; a From added above the signed one
/// fails too, because `h=` names From twice (oversigning). A field no signature covers may be
/// added, and a header block checked without its body ignores the body.
#[tokio::test]
async fn tampered_messages_fail() {
    let keys = rfc_keys();
    let mismatch = vec![Err(Failure::SignatureMismatch); 2];
    assert_eq!(
        results(&SIGNED.replace("hungry", "angry"), &keys).await,
        vec![Err(Failure::BodyHashMismatch); 2]
    );
    assert_eq!(
        results(&SIGNED.replace("dinner", "lunch"), &keys).await,
        mismatch
    );
    assert_eq!(
        results(
            &SIGNED.replace("From: Joe", "From: Mallory <m@evil.example>\r\nFrom: Joe"),
            &keys
        )
        .await,
        mismatch
    );
    assert_eq!(
        results(
            &SIGNED.replace("Message-ID:", "X-Mailer: any\r\nMessage-ID:"),
            &keys
        )
        .await,
        vec![Ok(()); 2]
    );
    let headers_only = verify_headers(SIGNED.replace("hungry", "angry").as_bytes(), &keys, now())
        .await
        .into_iter()
        .map(|verdict| verdict.result)
        .collect::<Vec<_>>();
    assert_eq!(headers_only, vec![Ok(()); 2]);
    let block = SIGNED.split("\r\n\r\n").next().unwrap().to_owned() + "\r\n";
    let alone = verify_headers(block.as_bytes(), &keys, now()).await;
    assert!(
        alone.iter().all(|verdict| verdict.result.is_ok()),
        "{alone:?}"
    );
}

/// What is refused whatever the key says: `rsa-sha1` (RFC 8301), a body length limit, a passed
/// expiry, `i=` outside `d=`, `h=` without From, a tag given twice; and what the key decides: none
/// published, revoked, of the other type, `t=s` with an `i=` below `d=`, or unreadable now (the
/// one temporary failure).
#[tokio::test]
async fn refuses_weak_or_unverifiable_signatures() {
    let keys = rfc_keys();
    let first = async |message: String, keys: &HashMap<String, String>| {
        results(&message, keys).await.into_iter().next().unwrap()
    };
    let second = results(&SIGNED.replacen("a=rsa-sha256", "a=rsa-sha1", 1), &keys).await;
    assert!(
        matches!(second[1], Err(Failure::Unsupported(_))),
        "{second:?}"
    );
    for (edit, expected) in [
        (("s=brisbane;", "l=10; s=brisbane;"), "unsupported"),
        (
            (
                "t=1528637909; h=from : to :",
                "t=1528637909; x=1528637910; h=from : to :",
            ),
            "expired",
        ),
        (("i=@football.example.com", "i=@example.net"), "malformed"),
        (
            (
                "h=from : to :\r\n subject : date : message-id : from : subject : date;",
                "h=to : subject;",
            ),
            "malformed",
        ),
        (("s=brisbane;", "s=brisbane; s=brisbane;"), "malformed"),
    ] {
        let outcome = first(SIGNED.replacen(edit.0, edit.1, 1), &keys).await;
        let kind = match outcome {
            Err(Failure::Unsupported(_)) => "unsupported",
            Err(Failure::Expired) => "expired",
            Err(Failure::Malformed(_)) => "malformed",
            ref other => panic!("{edit:?}: {other:?}"),
        };
        assert_eq!(kind, expected, "{edit:?}");
    }

    let mut missing = rfc_keys();
    missing.remove("brisbane._domainkey.football.example.com");
    let revoked = HashMap::from([(
        "brisbane._domainkey.football.example.com".to_owned(),
        "v=DKIM1; k=ed25519; p=".to_owned(),
    )]);
    let other_type = HashMap::from([(
        "brisbane._domainkey.football.example.com".to_owned(),
        RSA_RECORD.to_owned(),
    )]);
    for keys in [missing, revoked, other_type] {
        let outcome = first(SIGNED.to_owned(), &keys).await;
        assert!(
            matches!(outcome, Err(Failure::KeyInvalid(_))),
            "{outcome:?}"
        );
    }
    let strict = HashMap::from([(
        "brisbane._domainkey.football.example.com".to_owned(),
        format!("{ED25519_RECORD}; t=s"),
    )]);
    let below = SIGNED.replacen("i=@football.example.com", "i=@news.football.example.com", 1);
    assert!(matches!(
        first(below, &strict).await,
        Err(Failure::Malformed(_))
    ));
    let down = verify(SIGNED.as_bytes(), &Down, now()).await;
    assert!(
        down.iter()
            .all(|verdict| matches!(verdict.result, Err(Failure::KeyUnavailable(_)))),
        "{down:?}"
    );
}

/// A key record's tags (RFC 6376 §3.6.1): the type defaults to RSA, `p=` may be folded, `t=s` is
/// strict, an empty `p=` is a revoked key; a record that allows no SHA-256, serves no email,
/// repeats a tag or is another version is refused.
#[test]
fn reads_key_records() {
    let rsa = KeyRecord::parse("v=DKIM1; p=AQ AB").unwrap();
    assert_eq!(
        (rsa.kind.as_str(), rsa.key.as_slice(), rsa.strict),
        ("rsa", [1_u8, 0, 1].as_slice(), false)
    );
    let ed = KeyRecord::parse(&format!("{ED25519_RECORD}; t=y:s; s=email; h=sha256")).unwrap();
    assert_eq!(
        (ed.kind.as_str(), ed.key.len(), ed.strict),
        ("ed25519", 32, true)
    );
    assert!(KeyRecord::parse("v=DKIM1; p=").unwrap().key.is_empty());
    for refused in [
        "v=DKIM1; h=sha1; p=AQAB",
        "v=DKIM1; s=tlsrpt; p=AQAB",
        "v=DKIM2; p=AQAB",
        "p=AQAB; p=AQAB",
        "v=DKIM1; k=rsa",
        "v=DKIM1; p=not base64!",
    ] {
        assert!(
            matches!(KeyRecord::parse(refused), Err(Failure::KeyInvalid(_))),
            "{refused}"
        );
    }
}

/// What `sign` writes verifies under both algorithms, also once every CRLF became a bare LF, and
/// `signers` names each signature's domain and selector from the top: the signer the tests of
/// other crates use agrees with this verifier.
#[tokio::test]
async fn signed_messages_verify_and_name_their_signers() {
    let message = "From: Ada <ada@example.com>\r\nTo: grace@example.org\r\nSubject: Hello\r\nMessage-ID: <m1@example.com>\r\n\r\nHi.\r\n";
    let ed = Ed25519KeyPair::generate().unwrap();
    let rsa = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
    let keys = HashMap::from([
        (
            "ed._domainkey.example.com".to_owned(),
            format!(
                "v=DKIM1; k=ed25519; p={}",
                STANDARD.encode(ed.public_key().as_ref())
            ),
        ),
        (
            "rsa._domainkey.example.com".to_owned(),
            format!(
                "v=DKIM1; k=rsa; p={}",
                STANDARD.encode(rsa.public_key().as_der().unwrap().as_ref())
            ),
        ),
    ]);
    let once = sign(
        message.as_bytes(),
        &SigningKey::Ed25519(ed),
        "example.com",
        "ed",
        &["from", "to", "subject", "message-id"],
    )
    .unwrap();
    let twice = sign(
        &once,
        &SigningKey::Rsa(rsa),
        "example.com",
        "rsa",
        &["from", "subject", "message-id"],
    )
    .unwrap();
    let verdicts = verify(&twice, &keys, now()).await;
    assert_eq!(verdicts.len(), 2);
    assert!(
        verdicts.iter().all(|verdict| verdict.result.is_ok()),
        "{verdicts:?}"
    );
    assert_eq!(
        signers(&twice),
        [
            ("example.com".to_owned(), "rsa".to_owned()),
            ("example.com".to_owned(), "ed".to_owned()),
        ]
    );
    let bare = String::from_utf8(twice).unwrap().replace("\r\n", "\n");
    let verdicts = verify(bare.as_bytes(), &keys, now()).await;
    assert!(
        verdicts.iter().all(|verdict| verdict.result.is_ok()),
        "{verdicts:?}"
    );
}
