CREATE TABLE tenant_email_policy (
    tenant_id TEXT PRIMARY KEY,
    auto_stop BIGINT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE
);
CREATE TABLE tenant_email_usage (
    tenant_id TEXT PRIMARY KEY,
    report TEXT NOT NULL,
    sampled_at TEXT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE
);
