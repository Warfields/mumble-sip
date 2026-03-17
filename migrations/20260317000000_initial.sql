CREATE TABLE IF NOT EXISTS callers (
    phone_number TEXT PRIMARY KEY NOT NULL UNIQUE,
    nickname     TEXT NOT NULL,
    last_seen    INTEGER NOT NULL
);
