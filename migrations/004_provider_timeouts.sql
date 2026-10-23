-- Per-provider timeout overrides.
--
-- `NULL` inherits the global `XLITE_UPSTREAM_HEADER_TIMEOUT_SECS` /
-- `XLITE_UPSTREAM_STALL_TIMEOUT_SECS`, so existing rows keep today's behaviour.
-- A slow or flaky provider can now be bounded on its own without waiting for a
-- global setting that would also hurt everyone else.
ALTER TABLE providers ADD COLUMN header_timeout_secs INTEGER;
ALTER TABLE providers ADD COLUMN stall_timeout_secs  INTEGER;
