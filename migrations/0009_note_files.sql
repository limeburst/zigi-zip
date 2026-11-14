-- A collaborative note can hold files. Each file keeps its own link and short
-- code; `note_link_id` only says which note's file list it shows up in.
ALTER TABLE files ADD COLUMN IF NOT EXISTS note_link_id BIGINT
    REFERENCES links(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS files_note_link_id_idx ON files(note_link_id);

-- True when the file came in through a note's link without a password. Such
-- files are filed under the note's owner, and are only ever served as a
-- download so nobody can publish a page on this domain through a shared link.
ALTER TABLE files ADD COLUMN IF NOT EXISTS via_link BOOLEAN NOT NULL DEFAULT false;

-- Set by the note's owner: lets anyone holding the note's link upload files
-- into it, without a password.
ALTER TABLE pastes ADD COLUMN IF NOT EXISTS open_uploads BOOLEAN NOT NULL DEFAULT false;
