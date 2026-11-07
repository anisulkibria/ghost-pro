CREATE TABLE tenant_email_policy (
    tenant_id VARCHAR(24) PRIMARY KEY,
    auto_stop BIGINT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE
);
CREATE TABLE tenant_email_usage (
    tenant_id VARCHAR(24) PRIMARY KEY,
    report VARCHAR(8192) NOT NULL,
    sampled_at VARCHAR(40) NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE
);
