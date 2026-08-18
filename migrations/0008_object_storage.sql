-- Blobs now live in Cloudflare R2 instead of the local filesystem. The column
-- that held an absolute on-disk path now holds an R2 object key, which is the
-- blob's SHA-256 content hash — rename it so a stale binary cannot mistake one
-- for the other.
ALTER TABLE files RENAME COLUMN stored_path TO object_key;

-- Rewrite the legacy '/Users/.../zigi-data/files/<hash>' paths to bare keys.
-- Pre-dedup rows were backfilled with a content_hash long before this, so the
-- basename and the hash agree; fall back to the basename if one slipped through.
UPDATE files
   SET object_key = COALESCE(content_hash, regexp_replace(object_key, '^.*/', ''))
 WHERE object_key LIKE '/%';

CREATE INDEX IF NOT EXISTS files_object_key_idx ON files(object_key);
