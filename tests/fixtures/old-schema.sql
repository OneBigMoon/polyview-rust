-- Non-internal DDL from sqlite_schema in the November 20 polyview.sqlite3.
-- The snapshot had nine tables and three explicit indexes. No user data is included.
CREATE TABLE users (
    id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('admin','user')),
    created_at INTEGER NOT NULL
);
CREATE TABLE sessions (
    token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expiry ON sessions(expires_at);
CREATE TABLE login_attempts (
    username TEXT PRIMARY KEY, attempts INTEGER NOT NULL, started_at INTEGER NOT NULL
);
CREATE TABLE library_series (
    id INTEGER PRIMARY KEY, title TEXT NOT NULL UNIQUE
);
CREATE TABLE library_media (
    id INTEGER PRIMARY KEY, title TEXT NOT NULL,
    relative_path TEXT NOT NULL, root TEXT NOT NULL,
    kind TEXT NOT NULL, series_id INTEGER REFERENCES library_series(id),
    season INTEGER, episode INTEGER, size INTEGER NOT NULL,
    available INTEGER NOT NULL DEFAULT 1,
    UNIQUE(root, relative_path)
);
CREATE TABLE library_user_media (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    media_id INTEGER NOT NULL REFERENCES library_media(id) ON DELETE CASCADE,
    favorite INTEGER NOT NULL DEFAULT 0,
    position REAL NOT NULL DEFAULT 0, duration REAL NOT NULL DEFAULT 0,
    watched INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, media_id)
);
CREATE TABLE library_subscriptions (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    series_id INTEGER NOT NULL REFERENCES library_series(id) ON DELETE CASCADE,
    PRIMARY KEY(user_id, series_id)
);
CREATE INDEX library_media_series ON library_media(series_id);
CREATE TABLE channels (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, group_name TEXT NOT NULL,
    logo TEXT NOT NULL, url TEXT NOT NULL UNIQUE
);
CREATE TABLE downloads (
    id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL, url TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
    bytes_written INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER, error TEXT,
    relative_path TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL
);
CREATE INDEX downloads_user ON downloads(user_id, id);
