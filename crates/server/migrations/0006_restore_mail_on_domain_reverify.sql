-- Mark the identities and receive bindings a domain's suspension disabled, so re-verifying the
-- domain restores only those: an identity the person explicitly disabled, or a folder they
-- stopped reading, keeps `suspended_by_domain = false` and stays off. See `suspend_mail` and
-- `restore_mail` in `senders::domains::usage`.
ALTER TABLE sender_identities ADD COLUMN suspended_by_domain boolean NOT NULL DEFAULT false;
ALTER TABLE receive_bindings ADD COLUMN suspended_by_domain boolean NOT NULL DEFAULT false;
