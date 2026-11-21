ALTER TABLE subscriptions ADD COLUMN provider_version TEXT NULL;

-- Retire the old provider's settings while keeping historical subscriptions.
DELETE FROM platform_settings WHERE `key` IN (
    'billing.lemonsqueezy.apiKey',
    'billing.lemonsqueezy.storeId',
    'billing.lemonsqueezy.testMode',
    'billing.lemonsqueezy.webhookSecret'
);

UPDATE platform_settings SET value = '"polar"'
WHERE `key` = 'billing.provider' AND value = '"lemonsqueezy"';
