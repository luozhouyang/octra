-- OpenAPI-generated HTTP tools.
--
-- `kind` distinguishes regular MCP tools discovered from an upstream from
-- HTTP tools generated from an OpenAPI document. HTTP tools carry their
-- method, path template, and request mapping in the new columns; regular MCP
-- tools keep the defaults so existing rows decode unchanged.

ALTER TABLE mcp_tools ADD COLUMN kind TEXT NOT NULL DEFAULT 'mcp';
ALTER TABLE mcp_tools ADD COLUMN http_method TEXT;
ALTER TABLE mcp_tools ADD COLUMN http_path TEXT;
ALTER TABLE mcp_tools ADD COLUMN http_config_json TEXT NOT NULL DEFAULT '{}';
