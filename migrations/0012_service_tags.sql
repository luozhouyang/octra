-- Kong services are taggable like the other Admin API entities.

ALTER TABLE services ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';
