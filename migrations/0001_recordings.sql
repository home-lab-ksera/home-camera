CREATE TABLE recordings (
    id text PRIMARY KEY,
    camera_id text NOT NULL,
    actual_start_at timestamptz NOT NULL,
    actual_end_at timestamptz NOT NULL,
    spool_path text NOT NULL,
    object_key text NOT NULL UNIQUE,
    size bigint NOT NULL CHECK (size>0),
    checksum text NOT NULL,
    state text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','uploading','retry','stored')),
    attempts integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    lease_until timestamptz,
    claim_token uuid,
    etag text,
    stored_at timestamptz,
    last_error text
);
CREATE INDEX recordings_due ON recordings(next_attempt_at,actual_start_at) WHERE state IN ('pending','retry');
