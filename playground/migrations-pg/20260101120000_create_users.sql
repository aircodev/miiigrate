-- migration: 20260101120000_create_users
-- miiigrate is forward-only: never edit this file after it has been applied.

CREATE TYPE mood AS ENUM ('happy', 'sad', 'curious');

CREATE TABLE users (
    id bigserial PRIMARY KEY,
    email text UNIQUE,
    mood mood NOT NULL DEFAULT 'curious',
    balance numeric(12, 2) NOT NULL DEFAULT 0,
    tags text[],
    metadata jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Deliberate slow statement: keeps the advisory xact lock held long enough
-- that a concurrent migrator provably serializes behind it (see
-- run-postgres.sh). ::text because void columns cannot cross the database
-- worker's value decoder.
SELECT pg_sleep(2)::text;
