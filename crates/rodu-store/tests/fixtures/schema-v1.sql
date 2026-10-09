-- Rodu schema 1, exactly as v0.3.0 created it, with invented sample data. Do not edit:
-- the migration test opens it to prove old workspaces upgrade.
CREATE TABLE principals (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
  name TEXT NOT NULL UNIQUE COLLATE NOCASE,
  owner_id TEXT REFERENCES principals(id)
);
CREATE TABLE collections (
  id TEXT PRIMARY KEY,
  key TEXT NOT NULL UNIQUE COLLATE NOCASE,
  name TEXT NOT NULL,
  preset TEXT NOT NULL,
  workflow TEXT NOT NULL,
  created_at TEXT NOT NULL,
  next_number INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE cycles (
  id TEXT PRIMARY KEY,
  collection_id TEXT NOT NULL REFERENCES collections(id),
  name TEXT NOT NULL,
  starts_on TEXT,
  ends_on TEXT,
  state TEXT NOT NULL CHECK (state IN ('planned', 'active', 'closed')),
  UNIQUE (collection_id, name COLLATE NOCASE)
);
CREATE TABLE items (
  id TEXT PRIMARY KEY,
  collection_id TEXT NOT NULL REFERENCES collections(id),
  number INTEGER NOT NULL,
  key TEXT NOT NULL UNIQUE,
  type TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  status TEXT NOT NULL,
  category TEXT NOT NULL,
  priority TEXT NOT NULL,
  assignee_id TEXT REFERENCES principals(id),
  parent_id TEXT REFERENCES items(id),
  cycle_id TEXT REFERENCES cycles(id),
  estimate REAL,
  rank TEXT NOT NULL,
  due_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  version INTEGER NOT NULL,
  UNIQUE (collection_id, number)
);
CREATE INDEX items_rank ON items (collection_id, rank);
CREATE INDEX items_assignee ON items (assignee_id);
CREATE INDEX items_cycle ON items (cycle_id);
CREATE INDEX items_parent ON items (parent_id);
CREATE VIRTUAL TABLE items_fts USING fts5 (item_id UNINDEXED, title, body);
CREATE TABLE comments (
  id TEXT PRIMARY KEY,
  item_id TEXT NOT NULL REFERENCES items(id),
  author_id TEXT NOT NULL REFERENCES principals(id),
  via_agent_id TEXT REFERENCES principals(id),
  body TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX comments_item ON comments (item_id, created_at);
CREATE TABLE links (
  id TEXT PRIMARY KEY,
  from_item_id TEXT NOT NULL REFERENCES items(id),
  kind TEXT NOT NULL,
  target TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE (from_item_id, kind, target)
);
CREATE INDEX links_target ON links (target);
CREATE TABLE events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE,
  request_id TEXT NOT NULL,
  actor_id TEXT NOT NULL,
  via_agent_id TEXT,
  action TEXT NOT NULL,
  target_id TEXT NOT NULL,
  before TEXT,
  after TEXT,
  at TEXT NOT NULL
);
CREATE INDEX events_target ON events (target_id, seq);
CREATE TABLE idempotency (
  key TEXT PRIMARY KEY,
  result TEXT NOT NULL
);
PRAGMA user_version = 1;
INSERT INTO principals VALUES ('p-ann', 'human', 'ann', NULL);
INSERT INTO collections VALUES ('c-demo', 'DEMO', 'Demo project', 'kanban',
  '{"initial":"Backlog","states":[{"name":"Backlog","category":"backlog"},{"name":"Done","category":"done"}],"transitions":[]}',
  '2026-01-01T00:00:00Z', 3);
INSERT INTO items VALUES ('i-1', 'c-demo', 1, 'DEMO-1', 'epic', 'Sample epic', 'Plan the garden', 'Backlog',
  'backlog', 'normal', 'p-ann', NULL, NULL, NULL, 'a0', NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1);
INSERT INTO items VALUES ('i-2', 'c-demo', 2, 'DEMO-2', 'task', 'Water tomatoes', 'Every morning', 'Done',
  'done', 'high', NULL, 'i-1', NULL, 2.0, 'a1', '2026-02-01T00:00:00Z', '2026-01-02T00:00:00Z', '2026-01-03T00:00:00Z', 3);
INSERT INTO items_fts (item_id, title, body) VALUES ('i-1', 'Sample epic', 'Plan the garden');
INSERT INTO items_fts (item_id, title, body) VALUES ('i-2', 'Water tomatoes', 'Every morning');
INSERT INTO comments VALUES ('m-1', 'i-2', 'p-ann', NULL, 'Done today', '2026-01-03T00:00:00Z');
INSERT INTO links VALUES ('l-1', 'i-2', 'relates', 'i-1', '2026-01-03T00:00:00Z');
