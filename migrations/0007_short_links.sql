ALTER TABLE links DROP CONSTRAINT IF EXISTS links_kind_check;
ALTER TABLE links ADD CONSTRAINT links_kind_check CHECK (kind IN ('file', 'code', 'url'));

CREATE TABLE IF NOT EXISTS short_links (
    link_id          BIGINT PRIMARY KEY REFERENCES links(id) ON DELETE CASCADE,
    destination_url  TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by       BIGINT REFERENCES passwords(id) ON DELETE RESTRICT,
    created_by_label TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS short_links_created_at_idx ON short_links(created_at DESC);
