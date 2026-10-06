-- Explicit mail and tracking intent. Tracking retains its existing hostname and
-- certificate lifecycle; a mail domain may reference a separate tracking domain.
ALTER TABLE sending_domains ADD COLUMN purpose text NOT NULL DEFAULT 'send';
UPDATE sending_domains SET purpose = CASE WHEN tracking_enabled THEN 'tracking' ELSE 'send' END;
ALTER TABLE sending_domains ADD CONSTRAINT sending_domain_purpose
    CHECK (purpose IN ('tracking', 'send', 'receive', 'send_receive'));
ALTER TABLE sending_domains ADD COLUMN tracking_domain_id uuid;
ALTER TABLE sending_domains ADD CONSTRAINT sending_domain_tracking_reference
    FOREIGN KEY (workspace_id, tracking_domain_id) REFERENCES sending_domains (workspace_id, id)
    ON DELETE SET NULL (tracking_domain_id);
ALTER TABLE sending_domains ADD CONSTRAINT sending_domain_separate_tracking
    CHECK (tracking_domain_id IS NULL OR (tracking_domain_id <> id AND purpose <> 'tracking'));

-- Keep the legacy tracking flag coherent during a rolling upgrade. It describes
-- this row's hostname, never a mail domain's separately attached tracking hostname.
CREATE FUNCTION synchronize_sending_domain_purpose() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.tracking_enabled AND NEW.purpose = 'send' THEN
            NEW.purpose := 'tracking';
        END IF;
    ELSIF NEW.purpose = OLD.purpose AND NEW.tracking_enabled <> OLD.tracking_enabled THEN
        NEW.purpose := CASE WHEN NEW.tracking_enabled THEN 'tracking'
                           WHEN OLD.purpose = 'tracking' THEN 'send' ELSE OLD.purpose END;
    END IF;
    NEW.tracking_enabled := NEW.purpose = 'tracking';
    RETURN NEW;
END;
$$;
CREATE TRIGGER synchronize_sending_domain_purpose
    BEFORE INSERT OR UPDATE OF purpose, tracking_enabled ON sending_domains
    FOR EACH ROW EXECUTE FUNCTION synchronize_sending_domain_purpose();
ALTER TABLE sending_domains ADD CONSTRAINT sending_domain_tracking_flag
    CHECK (tracking_enabled = (purpose = 'tracking'));
