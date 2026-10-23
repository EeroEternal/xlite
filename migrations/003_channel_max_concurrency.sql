-- Per-channel concurrency cap.
--
-- `NULL` keeps the previous behaviour (a channel is limited only by the upstream
-- connection pool). When set, requests beyond the cap are shed (failover, then
-- `503` + `Retry-After`) instead of queueing, so one hot provider key cannot
-- starve the rest of the catalog.
ALTER TABLE channels ADD COLUMN max_concurrency INTEGER;
