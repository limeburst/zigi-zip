-- A browser signed in with a password. The cookie holds a random token; only
-- its SHA-256 is kept here. Revoking or rotating the password ends its
-- sessions (see passwd.rs), and so does signing out.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash   TEXT PRIMARY KEY,
    password_id  BIGINT NOT NULL REFERENCES passwords(id) ON DELETE CASCADE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sessions_password_id_idx ON sessions(password_id);
