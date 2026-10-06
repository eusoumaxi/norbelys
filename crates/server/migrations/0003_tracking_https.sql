-- Certificate permission resolves a proven hostname before a tenant is known.
-- The lookup role sees only routing facts, never credentials or mailbox data.
GRANT SELECT (id, hostname, purpose, status, dns_checks, checked_at) ON sending_domains TO norbelys_lookup;
CREATE POLICY lookup_tracking_hostname ON sending_domains FOR SELECT TO norbelys_lookup USING (true);
CREATE FUNCTION tracking_domain_route(name text) RETURNS uuid
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp
RETURN (SELECT id FROM sending_domains
        WHERE hostname = name AND purpose = 'tracking'
          AND status IN ('pending_certificate', 'active', 'verifying')
          AND dns_checks @> '{"ownership": true, "tracking": true}'::jsonb
          AND checked_at > now() - interval '48 hours');
ALTER FUNCTION tracking_domain_route(text) OWNER TO norbelys_lookup;
REVOKE EXECUTE ON FUNCTION tracking_domain_route(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION tracking_domain_route(text) TO norbelys_app;
