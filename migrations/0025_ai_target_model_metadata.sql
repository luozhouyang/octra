-- AI-native model targets own the upstream model metadata that used to live in
-- the legacy llm_models table: context window, output limit, capability flags,
-- and pricing.

ALTER TABLE ai_model_targets ADD COLUMN context_window INTEGER;
ALTER TABLE ai_model_targets ADD COLUMN max_output_tokens INTEGER;
ALTER TABLE ai_model_targets ADD COLUMN capabilities_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE ai_model_targets ADD COLUMN pricing_json TEXT NOT NULL DEFAULT '{}';
