ALTER TABLE llm_virtual_keys RENAME TO virtual_keys;
ALTER TABLE llm_usage RENAME TO ai_usage;
ALTER TABLE llm_usage_rollup RENAME TO ai_usage_rollup;
ALTER TABLE llm_budget_windows RENAME TO budget_windows;
ALTER TABLE llm_counters RENAME TO shared_counters;
ALTER TABLE llm_plugin_state RENAME TO plugin_state;
ALTER TABLE llm_pii_engines RENAME TO pii_engines;
