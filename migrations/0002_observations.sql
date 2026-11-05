-- raw_json is the merged, non-null catalog view. Exact original objects remain here.
CREATE TABLE catalog_observation (
 object_type TEXT NOT NULL, object_id TEXT NOT NULL, content_hash TEXT NOT NULL,
 raw_json TEXT NOT NULL CHECK(json_valid(raw_json)), observed_at TEXT NOT NULL,
 PRIMARY KEY(object_type,object_id,content_hash)
);
