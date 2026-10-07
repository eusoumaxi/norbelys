use super::*;
use crate::campaigns::creator;
use crate::testing::{SenderSpec, TestDb, keys};

fn invalid(detail: &str) -> MailboxFinding {
    MailboxFinding {
        status: MailboxStatus::Invalid,
        detail: detail.to_owned(),
    }
}

#[tokio::test]
async fn bulk_gate_includes_future_recipients_and_refusals_only_stop_the_enrollment() {
    let test = TestDb::new().await;
    let workspace = test.workspace("bulk-check").await.id;
    let sender = test
        .sender(workspace, &SenderSpec::relay("hello@bulk.example"))
        .await;
    let campaign = Id::from_uuid(test.campaign(workspace, &sender, None).await);
    for (email, seconds) in [("first@example.com", 0), ("future@example.com", 86400)] {
        sqlx::query(
            "WITH p AS (INSERT INTO people (workspace_id, email) VALUES ($1, $3) RETURNING id)
            INSERT INTO enrollments (workspace_id, campaign_id, person_id, next_run_at)
            SELECT $1, $2, id, now() + $4::integer * interval '1 second' FROM p",
        )
        .bind(workspace.uuid())
        .bind(campaign.uuid())
        .bind(email)
        .bind(seconds)
        .execute(test.system.pool())
        .await
        .unwrap();
    }
    let mut rows = pending(&test.worker, workspace, campaign).await.unwrap();
    assert_eq!(
        rows.len(),
        2,
        "future recipients belong to the initial bulk check"
    );
    rows.sort_by(|a, b| a.email.cmp(&b.email));
    let future = rows.pop().unwrap();
    let first = rows.pop().unwrap();
    let mut tx = test.worker.begin_in(workspace).await.unwrap();
    save(
        &mut tx,
        workspace,
        campaign,
        &[(
            first,
            MailboxFinding {
                status: MailboxStatus::Accepted,
                detail: "250 2.1.5 OK".to_owned(),
            },
        )],
    )
    .await
    .unwrap();
    assert!(!ready(&mut tx, workspace, campaign).await.unwrap());
    let blocked = creator::pass(
        &mut tx,
        &keys(),
        workspace,
        Some(campaign),
        jiff::Timestamp::now(),
        None,
        creator::CHUNK,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        blocked.created, 0,
        "the periodic creator cannot bypass the bulk gate"
    );
    save(
        &mut tx,
        workspace,
        campaign,
        &[(future, invalid("550 5.1.1 No mailbox"))],
    )
    .await
    .unwrap();
    assert!(ready(&mut tx, workspace, campaign).await.unwrap());
    let adding = jobs::enqueue(
        &mut tx,
        workspace,
        &crate::campaigns::enrollments::EnrollmentAdd {
            campaign: campaign.uuid(),
            audience: crate::campaigns::enrollments::Audience::People(Vec::new()),
            request: "bulk-still-loading".to_owned(),
        },
        None,
    )
    .await
    .unwrap();
    assert!(
        !ready(&mut tx, workspace, campaign).await.unwrap(),
        "wait for the entire audience to finish loading"
    );
    sqlx::query("UPDATE jobs SET state = 'completed', finished_at = now() WHERE id = $1")
        .bind(adding.uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(ready(&mut tx, workspace, campaign).await.unwrap());
    let made = creator::pass(
        &mut tx,
        &keys(),
        workspace,
        Some(campaign),
        jiff::Timestamp::now(),
        None,
        creator::CHUNK,
        true,
    )
    .await
    .unwrap();
    assert_eq!(made.created, 1);
    tx.commit().await.unwrap();
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM enrollments WHERE workspace_id = $1 ORDER BY status",
    )
    .bind(workspace.uuid())
    .fetch_all(test.system.pool())
    .await
    .unwrap();
    assert_eq!(statuses, ["active", "failed"]);
    let suppressions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM suppressions WHERE workspace_id = $1")
            .bind(workspace.uuid())
            .fetch_one(test.system.pool())
            .await
            .unwrap();
    assert_eq!(suppressions, 0, "a probe is not bounce evidence");
}

#[tokio::test]
async fn changed_addresses_discard_old_findings_and_unknown_allows_creation() {
    let test = TestDb::new().await;
    let workspace = test.workspace("changed-check").await.id;
    let sender = test
        .sender(workspace, &SenderSpec::relay("hello@changed.example"))
        .await;
    let campaign = Id::from_uuid(test.campaign(workspace, &sender, None).await);
    sqlx::query("WITH p AS (INSERT INTO people (workspace_id, email) VALUES ($1, 'old@example.com') RETURNING id)
        INSERT INTO enrollments (workspace_id, campaign_id, person_id, next_run_at) SELECT $1, $2, id, now() FROM p")
        .bind(workspace.uuid()).bind(campaign.uuid()).execute(test.system.pool()).await.unwrap();
    let old = pending(&test.worker, workspace, campaign)
        .await
        .unwrap()
        .pop()
        .unwrap();
    sqlx::query("UPDATE people SET email = 'new@example.com' WHERE workspace_id = $1")
        .bind(workspace.uuid())
        .execute(test.system.pool())
        .await
        .unwrap();
    let mut tx = test.worker.begin_in(workspace).await.unwrap();
    save(
        &mut tx,
        workspace,
        campaign,
        &[(old, invalid("550 5.1.1 Old mailbox is gone"))],
    )
    .await
    .unwrap();
    assert!(
        !ready(&mut tx, workspace, campaign).await.unwrap(),
        "an old-address answer does not count"
    );
    tx.commit().await.unwrap();
    let new = pending(&test.worker, workspace, campaign)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(new.email, "new@example.com");
    let mut tx = test.worker.begin_in(workspace).await.unwrap();
    save(
        &mut tx,
        workspace,
        campaign,
        &[(
            new,
            MailboxFinding {
                status: MailboxStatus::Unknown,
                detail: "450 Greylisted".to_owned(),
            },
        )],
    )
    .await
    .unwrap();
    assert!(ready(&mut tx, workspace, campaign).await.unwrap());
    let made = creator::pass(
        &mut tx,
        &keys(),
        workspace,
        Some(campaign),
        jiff::Timestamp::now(),
        None,
        creator::CHUNK,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        made.created, 1,
        "an inconclusive check must not discard mail"
    );
    tx.commit().await.unwrap();
}
