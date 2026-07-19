-- migration: 20260102090000_add_audit
-- Exercises the SQL splitter against real Postgres: the $$ body contains
-- semicolons that must not split the function definition.

CREATE TABLE audit_log (
    id bigserial PRIMARY KEY,
    user_id bigint NOT NULL,
    action text NOT NULL,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION audit_users() RETURNS trigger AS $$
BEGIN
    INSERT INTO audit_log (user_id, action) VALUES (NEW.id, TG_OP);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER users_audit
AFTER INSERT OR UPDATE ON users
FOR EACH ROW EXECUTE FUNCTION audit_users();
