-- Denormalize the password label at creation time so attribution survives
-- password deletion, and enforce that rotation goes through revoke, not delete.

ALTER TABLE files ADD COLUMN IF NOT EXISTS created_by_label TEXT;
UPDATE files f SET created_by_label = COALESCE(p.label, 'unknown')
FROM passwords p
WHERE f.uploaded_by = p.id AND f.created_by_label IS NULL;
UPDATE files SET created_by_label = 'unknown' WHERE created_by_label IS NULL;
ALTER TABLE files ALTER COLUMN created_by_label SET NOT NULL;

ALTER TABLE files DROP CONSTRAINT IF EXISTS files_uploaded_by_fkey;
ALTER TABLE files ADD CONSTRAINT files_uploaded_by_fkey
    FOREIGN KEY (uploaded_by) REFERENCES passwords(id) ON DELETE RESTRICT;

ALTER TABLE pastes ADD COLUMN IF NOT EXISTS created_by_label TEXT;
UPDATE pastes ps SET created_by_label = COALESCE(p.label, 'unknown')
FROM passwords p
WHERE ps.created_by = p.id AND ps.created_by_label IS NULL;
UPDATE pastes SET created_by_label = 'unknown' WHERE created_by_label IS NULL;
ALTER TABLE pastes ALTER COLUMN created_by_label SET NOT NULL;

ALTER TABLE pastes DROP CONSTRAINT IF EXISTS pastes_created_by_fkey;
ALTER TABLE pastes ADD CONSTRAINT pastes_created_by_fkey
    FOREIGN KEY (created_by) REFERENCES passwords(id) ON DELETE RESTRICT;
