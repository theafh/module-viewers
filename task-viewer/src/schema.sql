CREATE TABLE IF NOT EXISTS repositories (
    repository_id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    scanned_at TEXT,
    last_error TEXT
);

CREATE TABLE IF NOT EXISTS tasks (
    task_id INTEGER PRIMARY KEY AUTOINCREMENT,
    repository_id INTEGER NOT NULL REFERENCES repositories(repository_id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    scope TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('finished', 'deferred', 'live')),
    created TEXT NOT NULL,
    updated TEXT NOT NULL,
    ship_date TEXT,
    leave_date TEXT,
    UNIQUE (repository_id, path)
);

CREATE INDEX IF NOT EXISTS tasks_repository_id ON tasks (repository_id);
