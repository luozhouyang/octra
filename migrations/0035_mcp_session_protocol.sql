-- Store the protocol revision negotiated during initialize. Later requests may
-- then reject a mismatched MCP-Protocol-Version header instead of only checking
-- that the value is supported.
ALTER TABLE mcp_sessions ADD COLUMN protocol_version TEXT;
