-- An upload the server refused, kept for the hour it counts against the
-- account's hourly allowance (kiewer_lib::rate_limit). Refusing costs the
-- server the work of reading, and a refusal makes no revision, so without
-- this a hostile upload could repeat freely. An account's own expired rows
-- go when its next refusal is recorded, and refusals stop being recorded
-- once the allowance is used, so a row count stays small. The rows are
-- interchangeable, so no key beyond the index that reads them.
CREATE TABLE upload_refusal (
    account_id bigint NOT NULL REFERENCES account ON DELETE CASCADE,
    refused_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX upload_refusal_account_refused_at ON upload_refusal (account_id, refused_at);
