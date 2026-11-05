CREATE TABLE playlist_job (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES account(id),
 kind TEXT NOT NULL, name TEXT NOT NULL, public INTEGER NOT NULL DEFAULT 0,
 options_json TEXT NOT NULL CHECK(json_valid(options_json)),
 state TEXT NOT NULL DEFAULT 'ready', playlist_id TEXT,
 confirmed_count INTEGER NOT NULL DEFAULT 0, inflight_count INTEGER NOT NULL DEFAULT 0,
 total_items INTEGER NOT NULL, last_error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE playlist_job_item (
 job_id TEXT NOT NULL REFERENCES playlist_job(id), position INTEGER NOT NULL,
 musica_id TEXT NOT NULL REFERENCES musica(id), uri TEXT NOT NULL,
 PRIMARY KEY(job_id,position), UNIQUE(job_id,uri)
);
CREATE INDEX playlist_pending ON playlist_job(state,created_at);
