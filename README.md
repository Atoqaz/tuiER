# tuiER (`tuier`)

Interactive ER diagram of a SQLite database in the terminal (Rust + ratatui).
Runs fully locally. The database is opened read-only, except for the moment
you save a description or a table position.

## Install

```sh
cargo install --path .     # installs the `tuier` command to ~/.cargo/bin
tuier path/to/db.sqlite
```

Try it with the bundled example: `sqlite3 example/example.db < example/example.sql && tuier example/example.db`

## The diagram

- One box per table: key marker, column, data type, shortened description.
- `PK` yellow, `FK` cyan (`PF` = both). Lines run from each foreign key to the
  referenced column, `▶` pointing at the target.
- Referenced tables are placed left, referencing tables right.
- The bottom edge of each box shows its row count.
- Tables can be given a colour (in the table form, or menu → colour).
- The view reloads by itself when another program changes the database
  (it waits while you're in a form, and keeps your selection).

## Keys (press `?` in the viewer)

| Select       |                                                                                                                                             |
|--------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| arrows       | move the selection: ↑↓ through a table's rows (title → columns → next table), ←→ to the table on the left/right, landing on the nearest row |
| click, `tab` | select a column (click a title to select the table; `shift+tab` goes back); click again to close                                            |
| `/`          | search tables and columns; `tab` / ↑↓ cycle matches, `enter` keeps, `esc` cancels                                                           |
| `f`          | follow: FK → referenced column; on a PK → the referencing columns (press again to cycle)                                                    |
| `b`          | back to the previous selection                                                                                                              |
| `esc`        | clear the selection      

| Document            |                                                                                                                                                        |
|---------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------|
| `e`                 | on a **column**: edit its description (`enter` save, `esc` discard, `ctrl+u` clear; empty = remove). On a **table**: open the table editor (see below) |
| `n`                 | next table or column without a description; opens the description editor (`shift+n`: previous)                                                         |
| `tab` while editing | save and jump to the next undocumented item                                                                                                            |


| Tables                 |                                                                                                                                                                                                      |
|------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `m` or right-click     | table menu: `e` edit, `c` colour, `d` duplicate (structure only), `r` duplicate with rows, `x` delete, `p` preview, `i` schema, `q` query, `z` focus, `space` collapse (or ↑↓ / `w` `s` and `enter`) |
| `t` or **+ New table** | open the table form (see below); `enter` creates the table                                                                                                                                           |
| `e` on a table         | edit the table in the same form: columns, constraints, indexes                                                                                                                                       |
| `del`                  | delete the selected table, after asking (says which tables point at it)                                                                                                                              |


| Inspect          |                                                                                                          |
|------------------|----------------------------------------------------------------------------------------------------------|
| `:` or **Query** | query console (see below)                                                                                |
| `!` or **Lint**  | schema checks (see below); the button shows how many were found                                          |
| `p`              | preview the first 50 rows and the row count                                                              |
| `i`              | `CREATE` statement and all indexes                                                                       |
| `x`              | export next to the database: `m` Mermaid, `g` Graphviz, `d` Markdown dictionary, `s` SQL script, `a` all |

| Arrange          |                                                                                                             |
|------------------|-------------------------------------------------------------------------------------------------------------|
| `z`              | focus mode: only the selected table and its direct neighbours; moving the selection moves the focus         |
| `space`          | collapse / expand the selected table (only key columns stay visible)                                        |
| `-` / `+`        | collapse / expand all tables                                                                                |
| `shift` + arrows | move the selected table one grid cell (swaps with a table in the way); `shift` + `w` `a` `s` `d` also works |
| drag a title     | move a table with the mouse: live preview, `esc` cancels                                                    |
| `u`              | undo a layout change (`shift+u`: redo; also `ctrl+z` / `ctrl+y` and the toolbar buttons)                    |
| `shift+r`        | reset to the automatic layout (undoable)                                                                    |


| View                                          |                                                                                           |
|-----------------------------------------------|-------------------------------------------------------------------------------------------|
| `w` `a` `s` `d`, `h` `j` `k` `l`, drag, wheel | pan (`shift`+wheel horizontal, `shift+h` / `shift+l` half a page, `pgup` / `pgdn` a page) |
| `c` / `enter`                                 | center on the selection                                                                   |
| `g` or `home`                                 | top-left (`shift+g` or `end`: bottom-right)                                               |
| `?`                                           | this list of keys                                                                         |
| `q` / `ctrl+c`                                | quit                                                                                      |


## Toolbar

The top bar has clickable buttons: **+ New table** (`t`), **Query** (`:`),
**Lint** (`!`), **Undo** (`u`), **Redo** (`shift+u`), **Reset layout**
(`shift+r`) and **? Help** (`?`). Lint turns red for errors and yellow for
warnings. Undo/Redo show how many steps are available and are greyed out
when there is nothing to undo; Reset layout is greyed out while the layout
is the automatic one, and New table in `--read-only` mode.

## Moving tables

Hovering a title highlights it as a drag handle. While dragging, the diagram
shows the result live, the table gets a double yellow border, and the footer
says where it will land (and which table it swaps with). Dragging against
the edge of the view scrolls it. Esc cancels, releasing drops.

The mouse pointer changes to a hand/grab shape in terminals that support the
OSC 22 escape code (kitty, foot, xterm, WezTerm, Ghostty). Windows Terminal
and iTerm2 ignore it; the in-diagram feedback works everywhere. Disable it
with `--no-pointer-shape`.

## Table form

`t` opens the form (`e` on a table opens it filled in). At the top: the
table's name, description and **colour** (`space` opens a list with
swatches: white, red, green, yellow, blue, cyan, dark gray; white is the
default. Magenta isn't offered: it marks the selected table).

Below that, one row per column, every field in its own cell:

| Field | |
|---|---|
| Column | the name |
| Type | type it, or press `space` for a menu by category (Integer, Text, Real, Numeric, Binary). Types that take a number ask for it: NVARCHAR → length `15` → `NVARCHAR(15)`; DECIMAL → `10,2` |
| PK | primary key; tick several for a composite key. An `INTEGER` PK is SQLite's rowid alias |
| NN, UQ | NOT NULL, UNIQUE |
| AI | AUTOINCREMENT (only on a single INTEGER primary key) |
| Default | `0`, `open` (becomes `'open'`), `NULL`, `CURRENT_TIMESTAMP`, or an expression like `datetime('now')` |
| Collate | `space` picks BINARY (exact), NOCASE (ignore case), RTRIM (ignore trailing spaces) |
| Check | a rule every row must pass, e.g. `price >= 0` |
| References | foreign key: `space` (or start typing) opens a picker of key columns, including this table's own for self-references. Picking one also copies its type; `backspace` clears it |
| On delete, On update | `space` picks none, CASCADE, SET NULL, SET DEFAULT, RESTRICT (only with a reference) |
| Description | stored in `_descriptions` together with the table |

The line under the grid explains the focused field, or the field under the
mouse.

Below the grid, **table constraints**: UNIQUE over several columns together
(`a, b ; c, d` for two sets) and a table-level CHECK. Then the table's
**indexes**: name (leave empty for an automatic one), columns (`last_name,
first_name`, `DESC` and `COLLATE` allowed), UQ for a unique index, and an
optional WHERE for a partial index.

Keys: `tab` / ←→ next field, ↑↓ row. Arrowing down from the last column adds
a new one; arrowing away from it while it's empty removes it again (blank
rows are ignored anyway). The same goes for indexes, below CHECK. `space`
(or `x`) toggles checkboxes; `space` opens the lists for type, colour,
collate, references and FK actions, where typing filters, ↑↓ moves and
`enter` / `tab` picks. On an index's columns, `space` (`ctrl+space` once
it's filled in) picks columns from a list. `ctrl+u` clears a text field,
`ctrl+d` deletes a row, `ctrl+↑↓` reorders columns, **`enter` creates the
table** (or saves your changes), `esc` cancels (asks again if you've typed
something). You can also click fields.

The generated SQL is shown live under the form. Anything wrong (missing
names, duplicates, a name that exists) is shown there instead, and so is
SQLite's error if it refuses the statement. The table, its indexes, its
descriptions and its colour are created in one transaction.

Foreign key actions only take effect when the program using the database
turns on `PRAGMA foreign_keys`.

## Editing a table

`e` on a selected table opens the same form, filled in. SQLite can only
rename tables and columns in place, so other changes rebuild the table the
way the [SQLite docs](https://sqlite.org/lang_altertable.html#otheralter)
describe: renames first (so other tables' foreign keys, indexes and views
follow along), then a new table is created, the rows copied, the old table
dropped and the new one renamed into place; indexes, triggers and views are
recreated and the foreign keys checked. It all happens in one transaction:
if anything fails, nothing is changed and the error is shown in the form.

- The form says what saving will do: renames, added columns, removed columns,
  new indexes, and whether the table has to be rebuilt.
- Changing only descriptions doesn't touch the table at all; renames alone
  and index changes are done in place, without a rebuild.
- Removing a column, or rebuilding a table that uses something the form has
  no field for (generated columns, WITHOUT ROWID, STRICT, DEFERRABLE,
  ON CONFLICT, foreign keys over several columns), asks for a second `enter`,
  because saving would remove it.
- Renaming a column also updates the CHECKs, UNIQUE sets and indexes that
  use it.
- Columns other tables point at can't be removed and must stay a primary key
  or UNIQUE. A new NOT NULL column needs a default when the table has rows.

The generated `CREATE TABLE` is shown live under the form. Anything wrong
(missing names, duplicates, a name that exists) is shown there instead,
and so is SQLite's error if it refuses the statement. The table and its
descriptions are created in one transaction.

## Duplicating and deleting tables

The menu offers **duplicate (structure only)** and **duplicate with rows**.
You're asked for the new name (`orders_copy` by default). The copy gets the
same columns, constraints, indexes (under new names) and descriptions; a
self-reference points at the copy. **Delete** asks first, tells you the row
count and which tables point at the table, and also removes its
descriptions, saved position and colour. There's no undo for deleting:
copy the database file first if unsure.

## Lint

`!` (or the **Lint** button, red for errors, yellow for warnings) checks:

- rows whose foreign key points to a row that doesn't exist;
- foreign keys pointing at a column that isn't a single primary key or
  UNIQUE (SQLite then fails with "foreign key mismatch");
- foreign key columns whose type differs from the column they point at;
- foreign key columns without an index (slow lookups and deletes);
- tables without a primary key, columns without a type, duplicate indexes;
- how many tables and columns have no description.

↑↓ (or `w` `s`) move through the findings, `enter` jumps to the table or
column concerned, `r` runs the checks again, `esc` closes.

## Query console

`:` (or the **Query** button) opens a console. Type a query and press
`enter`; results appear as a grid (up to 1000 rows). It's read-only: the
database is opened in a mode where writes fail. `↑↓` walks through earlier
queries, `pgup`/`pgdn` and the wheel scroll rows, `ctrl+←/→` scrolls
columns, `ctrl+u` clears, `esc` closes (the query and history stay). A fresh
console starts with `SELECT * FROM <selected table> LIMIT 100`.

## SQL export

`x` then `s` (or `tuier db.sqlite --export sql`) writes a script that recreates
the whole database structure: tables in dependency order with their original
`CREATE` statements, indexes, views, triggers, the descriptions, the
saved layout and the table colours. Names are written without quotes unless SQL needs them
(keywords like `"order"`, names with spaces). Quotes SQLite added itself,
for example when a table was renamed, are removed too. Row data is not included; use `sqlite3 db.sqlite .dump` for that.

```sh
sqlite3 new.db < example.sql
```

## Stored data

SQLite has no `COMMENT ON`, so descriptions live in a table in the same
database, created on the first edit:

```sql
CREATE TABLE _descriptions (
  table_name  TEXT NOT NULL,
  column_name TEXT,            -- NULL = description of the table itself
  description TEXT NOT NULL
);
```

Manually arranged positions are saved in `_erd_layout` (`shift+r` removes
it) and table colours in `_erd_colors`.
These tables are hidden from the diagram. Use `--read-only` to change nothing.

## Options

```
--desc-table <NAME>   descriptions table (default: _descriptions)
--desc-width <N>      width of the shortened description column (default: 24)
--select <T[.C]>      start with a table or column selected
--read-only           disable editing and layout saving
--dump                print the diagram as plain text and exit
--export <FORMAT>     print mermaid | dot | md | sql to stdout and exit
--no-pointer-shape    don't change the mouse pointer
```
