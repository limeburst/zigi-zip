-- Rename the link kind 'paste' to 'code'. The table name `pastes` stays as-is
-- (renaming it would break the existing binary mid-rollout for no gain).

ALTER TABLE links DROP CONSTRAINT IF EXISTS links_kind_check;
UPDATE links SET kind = 'code' WHERE kind = 'paste';
ALTER TABLE links ADD CONSTRAINT links_kind_check CHECK (kind IN ('file', 'code'));
