-- migration: 20260103080000_add_scores
-- String literal with a semicolon proves quote handling end-to-end.

ALTER TABLE users ADD COLUMN motto text NOT NULL DEFAULT 'carpe diem; carpe noctem';

CREATE INDEX idx_users_mood ON users (mood);
