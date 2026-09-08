ALTER TABLE tenants ADD COLUMN kind TEXT NOT NULL DEFAULT 'ORGANIZATION'
    CHECK (kind IN ('PERSONAL', 'ORGANIZATION'));

-- Match normalized signup addresses against legacy directory addresses too.
CREATE UNIQUE INDEX users_normalized_email_idx ON users (tenant_id, lower(btrim(email)));

CREATE TABLE workspace_invitations (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    issuer_id   UUID NOT NULL,
    email       TEXT NOT NULL,
    token_hash  TEXT NOT NULL UNIQUE,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ,
    accepted_at TIMESTAMPTZ,
    FOREIGN KEY (tenant_id, issuer_id) REFERENCES users (tenant_id, id) ON DELETE CASCADE
);

CREATE INDEX workspace_invitations_tenant_idx ON workspace_invitations (tenant_id, created_at);
