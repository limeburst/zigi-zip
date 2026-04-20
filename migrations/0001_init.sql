CREATE TABLE IF NOT EXISTS passwords (
    id           BIGSERIAL PRIMARY KEY,
    label        TEXT NOT NULL UNIQUE,
    bcrypt_hash  TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS files (
    id               BIGSERIAL PRIMARY KEY,
    short_code       TEXT NOT NULL UNIQUE,
    original_name    TEXT NOT NULL,
    stored_path      TEXT NOT NULL,
    content_type     TEXT,
    size_bytes       BIGINT NOT NULL,
    uploaded_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    uploaded_by      BIGINT REFERENCES passwords(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS files_short_code_idx ON files(short_code);
CREATE INDEX IF NOT EXISTS files_uploaded_at_idx ON files(uploaded_at DESC);
