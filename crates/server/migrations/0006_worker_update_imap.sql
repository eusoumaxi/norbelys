-- The revoke job (provider.norbelys.revoke) restores a shared service's IMAP settings
-- (cleared by suspend_mail) when a domain is re-onboarded before the job runs. The worker
-- role could already set the other columns a re-verification touches (status, status_detail,
-- checked_at, updated_at) but not imap, which suspend_mail clears from the api role.
GRANT UPDATE (imap) ON connections TO norbelys_worker;
