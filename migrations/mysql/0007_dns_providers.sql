-- DNS hosts a customer has connected once, so every new domain on them is set
-- up without pasting an API token again. One row per account and provider.
--
-- `secret` is the API token sealed with the platform's credential key (see
-- pro_core::secret_box); the key is a file beside the data, never in here.
-- `zones` is the JSON list of zones the token could see when last checked,
-- shown to the customer so they know which domains it covers.
CREATE TABLE dns_providers (
    id            VARCHAR(24) NOT NULL PRIMARY KEY,
    account_id    VARCHAR(24) NOT NULL,
    provider      VARCHAR(32) NOT NULL,
    secret        VARCHAR(1024) NOT NULL,
    hint          VARCHAR(32) NOT NULL DEFAULT '',
    zones         VARCHAR(4000) NOT NULL DEFAULT '[]',
    last_used_at  VARCHAR(32) NULL,
    last_error    VARCHAR(1000) NULL,
    created_at    VARCHAR(32) NOT NULL,
    updated_at    VARCHAR(32) NOT NULL,
    CONSTRAINT dns_providers_account_fk FOREIGN KEY (account_id)
        REFERENCES accounts(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE UNIQUE INDEX dns_providers_account_provider ON dns_providers (account_id, provider);
