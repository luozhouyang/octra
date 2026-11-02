ALTER TABLE upstreams ADD COLUMN hash_on_header TEXT NOT NULL DEFAULT 'X-Consumer-ID';
ALTER TABLE upstreams ADD COLUMN hash_fallback_header TEXT NOT NULL DEFAULT 'X-Consumer-ID';
ALTER TABLE upstreams ADD COLUMN hash_on_cookie TEXT NOT NULL DEFAULT 'kong';
ALTER TABLE upstreams ADD COLUMN hash_on_cookie_path TEXT NOT NULL DEFAULT '/';
ALTER TABLE upstreams ADD COLUMN hash_on_query_arg TEXT NOT NULL DEFAULT '';
ALTER TABLE upstreams ADD COLUMN hash_fallback_query_arg TEXT NOT NULL DEFAULT '';
ALTER TABLE upstreams ADD COLUMN hash_on_uri_capture TEXT NOT NULL DEFAULT '';
ALTER TABLE upstreams ADD COLUMN hash_fallback_uri_capture TEXT NOT NULL DEFAULT '';
