-- Consumer groups and consumer-group-scoped plugin instances.

CREATE TABLE consumer_groups (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    tags       TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);

CREATE TABLE consumer_group_consumers (
    consumer_id       TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    consumer_group_id TEXT NOT NULL REFERENCES consumer_groups(id) ON DELETE CASCADE,
    created_at        INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    PRIMARY KEY (consumer_id, consumer_group_id)
);
CREATE INDEX idx_consumer_group_consumers_group
    ON consumer_group_consumers(consumer_group_id);

ALTER TABLE plugin_instances
    ADD COLUMN consumer_group_id TEXT REFERENCES consumer_groups(id) ON DELETE CASCADE;

-- The uniqueness rule for plugin scopes must account for the new scope column.
DROP INDEX idx_plugin_instances_scope;
CREATE UNIQUE INDEX idx_plugin_instances_scope ON plugin_instances(
    name,
    IFNULL(instance_name, ''),
    IFNULL(service_id, ''),
    IFNULL(route_id, ''),
    IFNULL(consumer_id, ''),
    IFNULL(consumer_group_id, '')
);
CREATE INDEX idx_plugin_instances_consumer_group
    ON plugin_instances(consumer_group_id);
