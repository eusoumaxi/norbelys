use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;

use super::*;
use crate::testing::{reply, smtp_default, smtp_server};

fn verifier(policy: AddressPolicy) -> Verifier {
    let resolver =
        TokioResolver::builder_with_config(ResolverConfig::default(), TokioRuntimeProvider::new())
            .build()
            .unwrap();
    Verifier {
        connector: Connector::new(resolver.clone(), policy).unwrap(),
        resolver,
        hello: ClientId::Domain("mail.example.com".to_owned()),
    }
}

#[test]
fn only_explicit_recipient_refusals_are_invalid() {
    for (code, reply, expected) in [
        (250, "2.1.5 OK", Outcome::Accepted),
        (251, "2.1.5 Will forward", Outcome::Accepted),
        (252, "Cannot verify user", Outcome::Unknown),
        (550, "5.1.1 No such mailbox", Outcome::Invalid),
        (553, "5.1.3 Bad recipient address", Outcome::Invalid),
        (550, "User unknown", Outcome::Unknown),
        (
            550,
            "5.7.1 Blocked; another host returned 5.1.1",
            Outcome::Unknown,
        ),
        (550, "policy blocked: 5.1.1", Outcome::Unknown),
        (550, "5.2.2 Mailbox full", Outcome::Unknown),
        (450, "4.1.1 Try again later", Outcome::Unknown),
        (450, "5.1.1 Inconsistent temporary reply", Outcome::Unknown),
    ] {
        assert_eq!(
            recipient_reply(code, reply).outcome,
            expected,
            "{code} {reply}"
        );
    }
}

#[test]
fn mx_order_duplicates_null_and_implicit_routes() {
    assert_eq!(
        mail_hosts("example.com.", [].into_iter()),
        Some(vec!["example.com.".to_owned()])
    );
    assert_eq!(
        mail_hosts("example.com.", [(0, ".".to_owned())].into_iter()),
        None
    );
    let records = [(30, "c."), (20, "b."), (10, "a."), (40, "d."), (11, "a.")];
    assert_eq!(
        mail_hosts(
            "example.com.",
            records
                .into_iter()
                .map(|(rank, host)| (rank, host.to_owned()))
        ),
        Some(vec!["a.".to_owned(), "b.".to_owned(), "c.".to_owned()])
    );
}

#[tokio::test]
async fn probes_rcpt_and_quits_without_auth_or_data() {
    for (answer, expected) in [
        ("250 2.1.5 OK", Outcome::Accepted),
        ("550 5.1.1 Unknown mailbox", Outcome::Invalid),
    ] {
        let (port, transcript) = smtp_server(move |line| {
            if line.starts_with("RCPT") {
                reply(answer)
            } else {
                smtp_default(line)
            }
        })
        .await;
        let found = verifier(AddressPolicy::Any)
            .host(
                "127.0.0.1",
                port,
                &"postmaster@mail.example.com".parse().unwrap(),
                &"ada@example.com".parse().unwrap(),
            )
            .await;
        assert_eq!(found.outcome, expected, "{found:?}");
        assert!(transcript.has("EHLO mail.example.com"));
        assert!(transcript.has("MAIL FROM:<postmaster@mail.example.com>"));
        assert!(transcript.has("RCPT TO:<ada@example.com>"));
        assert!(transcript.has("QUIT"));
        assert!(!transcript.has("AUTH"));
        assert!(!transcript.has("DATA"));
    }
}

#[tokio::test]
async fn sender_rejection_is_not_a_recipient_rejection() {
    let (port, transcript) = smtp_server(|line| {
        if line.starts_with("MAIL") {
            reply("550 5.1.1 Sender unknown")
        } else {
            smtp_default(line)
        }
    })
    .await;
    let found = verifier(AddressPolicy::Any)
        .host(
            "127.0.0.1",
            port,
            &"postmaster@mail.example.com".parse().unwrap(),
            &"ada@example.com".parse().unwrap(),
        )
        .await;
    assert_eq!(found.outcome, Outcome::Unknown);
    assert!(!transcript.has("RCPT"));
    assert!(!transcript.has("DATA"));
}

#[tokio::test]
async fn public_checker_cannot_reach_a_private_mx() {
    let (port, transcript) = smtp_server(smtp_default).await;
    let found = verifier(AddressPolicy::PublicOnly)
        .host(
            "127.0.0.1",
            port,
            &"postmaster@mail.example.com".parse().unwrap(),
            &"ada@example.com".parse().unwrap(),
        )
        .await;
    assert_eq!(found.outcome, Outcome::Unknown);
    assert!(found.diagnostic.contains("private"));
    assert!(transcript.lines().is_empty());
}
