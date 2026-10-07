-- A campaign's bulk recipient check is durable per enrollment. A worker restart resumes the
-- unchecked rows; changing a person's address makes its previous check inapplicable. Kept
-- apart from the DNS route cache and from delivery/bounce evidence: no message was sent.
ALTER TABLE enrollments ADD COLUMN recipient_validation jsonb
    CHECK (recipient_validation IS NULL OR
           (jsonb_typeof(recipient_validation) = 'object'
            AND recipient_validation ?& ARRAY['email_key', 'status', 'checked_at', 'detail']
            AND recipient_validation ->> 'status' IN ('accepted', 'invalid', 'unknown', 'skipped')));

CREATE INDEX enrollments_recipient_validation ON enrollments (workspace_id, campaign_id, id)
    WHERE status = 'active' AND current_position = 1 AND message_id IS NULL;

-- The bulk gate also waits for the audience's unfinished enrollment jobs.
CREATE INDEX jobs_campaign_enrollment_add ON jobs (workspace_id, (payload ->> 'campaign'))
    WHERE kind = 'enrollment.add' AND state IN ('available', 'running');
