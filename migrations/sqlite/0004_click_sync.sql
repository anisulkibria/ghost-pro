CREATE TABLE click_sync (
 tenant_id VARCHAR(24) PRIMARY KEY,
 revision BIGINT NOT NULL DEFAULT 1,
 synced_revision BIGINT NOT NULL DEFAULT 0,
 enabled INTEGER NOT NULL DEFAULT 1,
 payload VARCHAR(4000),
 state VARCHAR(20) NOT NULL DEFAULT 'pending',
 attempts INTEGER NOT NULL DEFAULT 0,
 available_at VARCHAR(40) NOT NULL DEFAULT '',
 last_error VARCHAR(200),
 website_id VARCHAR(36),
 verification_token VARCHAR(100)
);
CREATE INDEX click_sync_due ON click_sync(state,available_at);
CREATE TRIGGER click_tenant_created AFTER INSERT ON tenants WHEN 1=1
BEGIN
 INSERT INTO click_sync(tenant_id) VALUES(NEW.id) ON CONFLICT(tenant_id) DO UPDATE SET revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='';
END;
CREATE TRIGGER click_tenant_changed AFTER UPDATE ON tenants WHEN NEW.status<>OLD.status OR NEW.plan_slug<>OLD.plan_slug OR NEW.name<>OLD.name
BEGIN
 INSERT INTO click_sync(tenant_id) VALUES(NEW.id) ON CONFLICT(tenant_id) DO UPDATE SET revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='';
END;
CREATE TRIGGER click_domain_created AFTER INSERT ON domains WHEN NEW.is_primary=1
BEGIN
 INSERT INTO click_sync(tenant_id) VALUES(NEW.tenant_id) ON CONFLICT(tenant_id) DO UPDATE SET revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='';
END;
CREATE TRIGGER click_domain_changed AFTER UPDATE ON domains WHEN NEW.is_primary<>OLD.is_primary OR NEW.status<>OLD.status OR NEW.domain<>OLD.domain
BEGIN
 INSERT INTO click_sync(tenant_id) VALUES(NEW.tenant_id) ON CONFLICT(tenant_id) DO UPDATE SET revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='';
END;
CREATE TRIGGER click_domain_removed AFTER DELETE ON domains WHEN OLD.is_primary=1
BEGIN
 INSERT INTO click_sync(tenant_id) VALUES(OLD.tenant_id) ON CONFLICT(tenant_id) DO UPDATE SET revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='';
END;
