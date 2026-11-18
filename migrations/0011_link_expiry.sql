-- A link that stops working at a time: the API's uploads, which get long,
-- unguessable codes and expire (default a week). Past it, the link answers
-- as if it never was, and the server deletes it (and its file, when no other
-- link has it) within a few minutes.
ALTER TABLE links ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS links_expires_at_idx ON links(expires_at) WHERE expires_at IS NOT NULL;
