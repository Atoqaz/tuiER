# Data dictionary: example.db

| Table | Columns | Description |
|---|---|---|
| [comments](#comments) | 4 |  |
| [organization](#organization) | 3 | A customer account; every user belongs to exactly one organization. |
| [pets](#pets) | 4 | my pets |
| [projects](#projects) | 4 | All my projects |
| [species](#species) | 2 |  |
| [tags](#tags) | 2 |  |
| [task_tags](#task_tags) | 2 | Many-to-many link between tasks and tags |
| [tasks](#tasks) | 4 |  |
| [users](#users) | 4 | People who can log in. |

## comments

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  |  |
| FK | `task_id` | INTEGER | no |  | `tasks.id` |  |
| FK | `author_id` | INTEGER | no |  | `users.id` |  |
|  | `body` | TEXT | yes |  |  | Markdown text |

## organization

A customer account; every user belongs to exactly one organization.

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  | Surrogate key |
|  | `name` | TEXT | no |  |  | Display name shown in the app header and on invoices |
|  | `created_at` | TEXT | yes | `CURRENT_TIMESTAMP` |  |  |

Referenced by: `pets.fk_id`, `projects.org_id`, `users.org_id`

## pets

my pets

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  | Primary |
| FK | `fk_id` | INTEGER | yes |  | `organization.id` | forein key |
|  | `name` | TEXT | yes |  |  |  |
| FK | `species` | INTEGER | yes |  | `species.id` |  |

Indexes:

- `pets_fk_id_idx` (index): fk_id

## projects

All my projects

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  |  |
| FK | `org_id` | INTEGER | no |  | `organization.id` |  |
| FK | `owner_id` | INTEGER | yes |  | `users.id` | User responsible for the project |
|  | `title` | TEXT | no |  |  |  |

Referenced by: `tasks.project_id`

## species

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  |  |
|  | `species` | TEXT | yes |  |  | Type of species. Dog, Rabbit, Crocodile |

Referenced by: `pets.species`

## tags

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  |  |
|  | `label` | TEXT | no |  |  |  |

Referenced by: `task_tags.tag_id`

## task_tags

Many-to-many link between tasks and tags

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK, FK | `task_id` | INTEGER | yes |  | `tasks.id` |  |
| PK, FK | `tag_id` | INTEGER | yes |  | `tags.id` |  |

Indexes:

- `sqlite_autoindex_task_tags_1` (primary key): task_id, tag_id

## tasks

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  | Task primary key |
| FK | `project_id` | INTEGER | no |  | `projects.id` | Task projekt id |
| FK | `assignee_id` | INTEGER | yes |  | `users.id` |  |
|  | `status` | TEXT | no | `'open'` |  | One of open, in_progress, blocked, done. Transitions are enforced by the application, not by the database. |

Referenced by: `comments.task_id`, `task_tags.task_id`

## users

People who can log in.

| Key | Column | Type | Null | Default | References | Description |
|---|---|---|---|---|---|---|
| PK | `id` | INTEGER | yes |  |  | Surrogate key |
| FK | `org_id` | INTEGER | no |  | `organization.id` | Organization the user belongs to; deleting the organization is blocked while users exist |
| UQ | `email` | TEXT | no |  |  | Login e-mail, unique across all organization |
| FK | `manager_id` | INTEGER | yes |  | `users.id` | Direct manager, NULL for top of the hierarchy |

Referenced by: `comments.author_id`, `projects.owner_id`, `tasks.assignee_id`, `users.manager_id`

Indexes:

- `sqlite_autoindex_users_1` (unique constraint): email
