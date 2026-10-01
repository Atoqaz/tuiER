-- The database the tests run against (see schema::test_db). Kept apart
-- from example.db, so editing the example doesn't break the tests.

CREATE TABLE organization (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE tags (id INTEGER PRIMARY KEY, label TEXT NOT NULL);

CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL REFERENCES organization(id),
  email TEXT NOT NULL UNIQUE,
  manager_id INTEGER REFERENCES users(id)
);

CREATE TABLE projects (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL REFERENCES organization,
  owner_id INTEGER REFERENCES users(id),
  title TEXT NOT NULL
);

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  assignee_id INTEGER REFERENCES users(id),
  status TEXT NOT NULL DEFAULT 'open'
);

CREATE TABLE comments (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  author_id INTEGER NOT NULL REFERENCES users(id),
  body TEXT
);

CREATE TABLE task_tags (
  task_id INTEGER REFERENCES tasks(id),
  tag_id INTEGER REFERENCES tags(id),
  PRIMARY KEY (task_id, tag_id)
);

CREATE TABLE _descriptions (
    table_name  TEXT NOT NULL,
    column_name TEXT,
    description TEXT NOT NULL
);
INSERT INTO _descriptions (table_name, column_name, description) VALUES
  ('comments', 'body', 'Markdown text'),
  ('organization', NULL, 'A customer account; every user belongs to exactly one organization.'),
  ('organization', 'id', 'Surrogate key'),
  ('organization', 'name', 'Display name shown in the app header and on invoices'),
  ('projects', NULL, 'All my projects'),
  ('projects', 'owner_id', 'User responsible for the project'),
  ('task_tags', NULL, 'Many-to-many link between tasks and tags'),
  ('tasks', 'id', 'Task primary key'),
  ('tasks', 'project_id', 'Task projekt id'),
  ('tasks', 'status', 'One of open, in_progress, blocked, done. Transitions are enforced by the application, not by the database.'),
  ('users', NULL, 'People who can log in.'),
  ('users', 'id', 'Surrogate key'),
  ('users', 'org_id', 'Organization the user belongs to; deleting the organization is blocked while users exist'),
  ('users', 'email', 'Login e-mail, unique across all organization'),
  ('users', 'manager_id', 'Direct manager, NULL for top of the hierarchy');
