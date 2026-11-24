-- DNS hosts a customer has connected once, so every new domain on them is set
-- up without pasting an API token again. One row per account and provider.
--
-- `secret` is the API token sealed with the platform's credential key (see
-- pro_core::secret_box); the key is a file beside the data, never in here.
-- `zones` is the JSON list of zones the token could see when last checked,
-- shown to the customer so they know which domains it covers.
CREATE TABLE dns_providers (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    provider      TEXT NOT NULL,
    secret        TEXT NOT NULL,
    hint          TEXT NOT NULL DEFAULT '',
    zones         TEXT NOT NULL DEFAULT '[]',
    last_used_at  TEXT,
    last_error    TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE UNIQUE INDEX dns_providers_account_provider ON dns_providers (account_id, provider);
