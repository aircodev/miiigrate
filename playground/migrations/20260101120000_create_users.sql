-- migration: 20260101120000_create_users
-- miiigrate is forward-only: never edit this file after it has been applied;
-- write a new migration instead.

CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
