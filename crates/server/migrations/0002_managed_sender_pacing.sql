-- Managed senders may opt into exact minute pacing. Other providers keep their
-- existing mailbox grid and relay constraints. Existing intervals and credentials
-- are preserved.
ALTER TABLE connections DROP CONSTRAINT connections_send_interval_minutes_check;

DO $$
DECLARE
    names text[];
BEGIN
    SELECT array_agg(conname::text) INTO names
      FROM pg_constraint
     WHERE conrelid = 'connections'::regclass AND contype = 'c'
       AND pg_get_constraintdef(oid) LIKE '%provider%'
       AND pg_get_constraintdef(oid) LIKE '%norbelys%'
       AND pg_get_constraintdef(oid) LIKE '%send_interval_minutes%';
    IF coalesce(cardinality(names), 0) <> 1 THEN
        RAISE EXCEPTION 'Expected one managed sender pacing constraint';
    END IF;
    EXECUTE format('ALTER TABLE connections DROP CONSTRAINT %I', names[1]);
END $$;

ALTER TABLE connections
    ADD CONSTRAINT connections_send_interval_minutes_check
        CHECK (send_interval_minutes IS NULL OR
               (send_interval_minutes BETWEEN 1 AND 1440 AND
                (provider = 'norbelys' OR send_interval_minutes >= 5))),
    ADD CONSTRAINT connections_unpaced_relays_check
        CHECK (provider NOT IN ('sendgrid', 'mailgun') OR send_interval_minutes IS NULL);

-- Project native sends at their exact minute cadence before grouping into dashboard slots.
CREATE OR REPLACE VIEW slot_projection WITH (security_invoker = true) AS
WITH slots AS (
    SELECT o AS slot_offset, date_bin('5 minutes', now(), TIMESTAMPTZ '2001-01-01 00:00:00+00') + o * interval '5 minutes' AS slot
      FROM generate_series(0, 11) o),
cold AS (
    SELECT c.workspace_id, c.id,
           date_bin('5 minutes', greatest(c.next_send_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00') AS first_slot,
           (c.send_interval_minutes + 4) / 5 AS every,
           row_number() OVER (PARTITION BY c.workspace_id, c.id ORDER BY q.run_at, q.message_id) AS k,
           greatest(0, (extract(epoch FROM date_bin('5 minutes', greatest(q.run_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00')
                                         - date_bin('5 minutes', greatest(c.next_send_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00')) / 300)::int) AS d
      FROM connections c
      JOIN delivery_queue q ON (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
     WHERE c.status = 'active' AND NOT c.paused AND c.send_interval_minutes IS NOT NULL
       AND c.provider <> 'norbelys'
       AND q.state IN ('queued', 'claimed') AND q.paced AND q.run_at < now() + interval '1 hour'),
paced AS (
    SELECT first_slot + ((k - 1) * every + max(d - (k - 1) * every) OVER (PARTITION BY workspace_id, id ORDER BY k)) * interval '5 minutes' AS slot
      FROM cold),
managed_cold AS (
    SELECT c.workspace_id, c.id, greatest(c.next_send_at, now()) AS first_at,
           make_interval(mins => c.send_interval_minutes) AS spacing,
           row_number() OVER (PARTITION BY c.workspace_id, c.id ORDER BY q.run_at, q.message_id) AS k,
           greatest(q.run_at, now()) AS due_at
      FROM connections c
      JOIN delivery_queue q ON (q.workspace_id, q.connection_id) = (c.workspace_id, c.id)
     WHERE c.provider = 'norbelys' AND c.status = 'active' AND NOT c.paused
       AND c.send_interval_minutes IS NOT NULL AND q.state IN ('queued', 'claimed')
       AND q.paced AND q.run_at < now() + interval '1 hour'),
managed_paced AS (
    SELECT date_bin('5 minutes',
               (k - 1) * spacing + greatest(first_at,
                   max(due_at - (k - 1) * spacing) OVER (PARTITION BY workspace_id, id ORDER BY k)),
               TIMESTAMPTZ '2001-01-01 00:00:00+00') AS slot
      FROM managed_cold),
due AS (
    SELECT date_bin('5 minutes', greatest(q.run_at, now()), TIMESTAMPTZ '2001-01-01 00:00:00+00') AS slot
      FROM delivery_queue q JOIN connections c ON (c.workspace_id, c.id) = (q.workspace_id, q.connection_id)
     WHERE q.state IN ('queued', 'claimed') AND q.run_at < now() + interval '1 hour' AND c.status = 'active' AND NOT c.paused
       AND (NOT q.paced OR c.send_interval_minutes IS NULL))
SELECT s.slot_offset, s.slot,
       ((SELECT count(*) FROM paced p WHERE p.slot = s.slot) +
        (SELECT count(*) FROM managed_paced p WHERE p.slot = s.slot)) AS paced,
       (SELECT count(*) FROM due d WHERE d.slot = s.slot) AS due
  FROM slots s;
