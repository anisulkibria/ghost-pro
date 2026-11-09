ALTER TABLE click_sync ADD COLUMN incarnation BIGINT NOT NULL DEFAULT 0;
DROP TRIGGER click_tenant_changed;
CREATE TRIGGER click_tenant_changed AFTER UPDATE ON tenants FOR EACH ROW
BEGIN
 IF NEW.status<>OLD.status OR NEW.plan_slug<>OLD.plan_slug OR NEW.name<>OLD.name THEN
 INSERT INTO click_sync(tenant_id,incarnation) VALUES(NEW.id,CASE WHEN OLD.status='deleted' AND NEW.status='active' THEN 1 ELSE 0 END)
 ON DUPLICATE KEY UPDATE revision=revision+1,payload=NULL,state='pending',attempts=0,available_at='',
 incarnation=incarnation+CASE WHEN OLD.status='deleted' AND NEW.status='active' THEN 1 ELSE 0 END;
 END IF;
END;
