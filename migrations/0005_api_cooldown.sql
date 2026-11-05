CREATE TABLE api_cooldown (
 id INTEGER PRIMARY KEY CHECK(id=1), retry_at TEXT NOT NULL, reason TEXT NOT NULL
);
