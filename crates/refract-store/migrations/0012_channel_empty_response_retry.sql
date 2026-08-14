-- Per-channel overrides for HTTP 200 empty-response retry behavior.
ALTER TABLE channels ADD COLUMN empty_response_retry TEXT;
