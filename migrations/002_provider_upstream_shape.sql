-- Provider upstream shape: previously the gateway could only speak `Bearer <key>`
-- to `<base_url>/v1/chat/completions`. Real upstreams differ on all three axes, so
-- they become catalog data instead of code.
--
-- Every column defaults to today's behaviour, so existing rows are unchanged.

-- `bearer` (default) | `raw` (api_key verbatim as the header value) | `none`.
ALTER TABLE providers ADD COLUMN auth_scheme   TEXT NOT NULL DEFAULT 'bearer';
-- Header name for the credential; empty means `authorization`.
ALTER TABLE providers ADD COLUMN auth_header   TEXT NOT NULL DEFAULT '';
-- Static extra request headers, JSON array of [name, value] pairs. The gateway
-- parses and validates these at snapshot build, never per request.
ALTER TABLE providers ADD COLUMN extra_headers TEXT NOT NULL DEFAULT '[]';
-- Override for the chat/completions path; empty keeps the derived default.
ALTER TABLE providers ADD COLUMN chat_path     TEXT NOT NULL DEFAULT '';
