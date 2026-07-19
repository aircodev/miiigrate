-- migration: 20260102090000_add_email
-- miiigrate is forward-only: never edit this file after it has been applied;
-- write a new migration instead.

ALTER TABLE users ADD COLUMN email TEXT;

CREATE UNIQUE INDEX idx_users_email ON users (email);
