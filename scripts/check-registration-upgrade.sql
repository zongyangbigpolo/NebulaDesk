-- Run before replacing/restarting the Manager:
--   psql "$NEBULA_DATABASE_URL" -X -f scripts/check-registration-upgrade.sql
-- Exit status is nonzero if the registration migration would reject legacy
-- email collisions. This script is read-only and never merges accounts.
-- Quiesce directory writes between this check and migration: a successful
-- preflight cannot prevent a concurrent legacy writer creating a new collision.
\set ON_ERROR_STOP on

BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;

SELECT tenant_id,
       lower(btrim(email)) AS normalized_email,
       array_agg(id ORDER BY id) AS conflicting_user_ids,
       count(*) AS account_count
FROM users
GROUP BY tenant_id, lower(btrim(email))
HAVING count(*) > 1
ORDER BY tenant_id, normalized_email;

DO $preflight$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM users
        GROUP BY tenant_id, lower(btrim(email))
        HAVING count(*) > 1
    ) THEN
        RAISE EXCEPTION 'Registration upgrade blocked: duplicate normalized emails in a tenant'
            USING HINT = 'Review the listed user IDs with their owners. Assign distinct, verified addresses through an explicitly approved operator procedure, then rerun this check. Do not merge or delete accounts automatically.';
    END IF;
END
$preflight$;

COMMIT;
