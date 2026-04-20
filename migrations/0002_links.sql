-- Unified short-code namespace for all link kinds (files, pastes, ...).
CREATE TABLE IF NOT EXISTS links (
    id          BIGSERIAL PRIMARY KEY,
    short_code  TEXT NOT NULL UNIQUE,
    kind        TEXT NOT NULL CHECK (kind IN ('file', 'paste')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Migrate existing files.short_code values into links, then replace the column with a FK.
INSERT INTO links (short_code, kind, created_at)
SELECT short_code, 'file', uploaded_at FROM files
ON CONFLICT (short_code) DO NOTHING;

ALTER TABLE files ADD COLUMN IF NOT EXISTS link_id BIGINT;

UPDATE files f SET link_id = l.id
FROM links l
WHERE f.short_code = l.short_code AND f.link_id IS NULL;

ALTER TABLE files ALTER COLUMN link_id SET NOT NULL;
ALTER TABLE files ADD CONSTRAINT files_link_id_fkey
    FOREIGN KEY (link_id) REFERENCES links(id) ON DELETE CASCADE;
ALTER TABLE files ADD CONSTRAINT files_link_id_unique UNIQUE (link_id);

DROP INDEX IF EXISTS files_short_code_idx;
ALTER TABLE files DROP CONSTRAINT IF EXISTS files_short_code_key;
ALTER TABLE files DROP COLUMN IF EXISTS short_code;

-- Live pastebin: state is a Yjs update stream; each row is one paste.
CREATE TABLE IF NOT EXISTS pastes (
    link_id     BIGINT PRIMARY KEY REFERENCES links(id) ON DELETE CASCADE,
    doc_state   BYTEA NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by  BIGINT REFERENCES passwords(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS pastes_updated_at_idx ON pastes(updated_at DESC);
