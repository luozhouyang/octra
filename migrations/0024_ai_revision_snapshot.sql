-- Store the complete control-plane snapshot on each revision so a revision can
-- be rolled back without reconstructing it from the diff.

ALTER TABLE ai_config_revisions ADD COLUMN snapshot_json TEXT NOT NULL DEFAULT '{}';
