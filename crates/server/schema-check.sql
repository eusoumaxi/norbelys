-- One read-only compatibility contract, used by the deployed server and external tooling.
-- $1/$2 are the versions/checksums produced by SQLx from the authoritative migration files.
WITH expected AS (
    SELECT * FROM unnest($1::bigint[], $2::bytea[]) AS migration(version, checksum)
)
SELECT NOT EXISTS (
    SELECT 1 FROM expected e FULL JOIN public._sqlx_migrations a USING (version)
    WHERE e.version IS NULL OR a.version IS NULL OR NOT a.success OR e.checksum <> a.checksum
)
