-- The platform's own way into each site's Admin API.
--
-- At setup time the platform holds the owner's credentials for one moment;
-- it uses that moment to create a custom integration on the site and keep
-- the integration's admin key here. From then on the platform reaches the
-- site's API as that integration — the same mechanism any customer would
-- use to connect Zapier — and holds no password.
--
-- A site owner who deletes the integration revokes the platform's access,
-- which is the property that makes storing a key here acceptable where
-- storing a password never would be.
ALTER TABLE tenants ADD COLUMN platform_key_id TEXT;
ALTER TABLE tenants ADD COLUMN platform_key_secret TEXT;
