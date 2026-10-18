# Sequoia

A minimal disk-based relational database written in Rust with zero dependencies.
It is meant to be small and easy to read, not fast.

## Usage

```bash
cargo run                # in-memory database
cargo run -- my.db       # persisted to my.db
```

```sql
CREATE TABLE users (id INT PRIMARY KEY, name TEXT, active BOOL);
INSERT INTO users VALUES (1, 'alice', TRUE), (2, 'bob', FALSE);
SELECT name FROM users WHERE active AND id > 0 ORDER BY name DESC LIMIT 10;
UPDATE users SET active = NOT active WHERE id = 2;
DELETE FROM users WHERE name = 'alice';
DROP TABLE users;
```

Shell commands: `.tables`, `.exit`.

## Supported SQL

- Statements: `CREATE TABLE`, `DROP TABLE`, `INSERT`, `SELECT`, `UPDATE`, `DELETE`
- Types: `INT`, `TEXT`, `BOOL`, and `NULL`
- Constraints: `PRIMARY KEY` (unique, not null); values are type-checked
- `SELECT`: `*` or expressions, `WHERE`, `ORDER BY ... ASC|DESC`, `LIMIT`
- Expressions: `+ - * /`, `= != <> < <= > >=`, `AND OR NOT`, `IS [NOT] NULL`,
  parentheses, with SQL three-valued logic for `NULL`

Not supported: joins, aggregates, indexes, transactions.

## Layout

```
src/
  main.rs       interactive shell
  lib.rs        module list and public exports
  database.rs   entry point; runs SQL, saves and syncs after changes
  lexer.rs      SQL text -> tokens
  parser.rs     tokens -> AST (recursive descent)
  ast.rs        statement and expression types
  executor.rs   runs statements, evaluates expressions
  catalog.rs    table schemas and where each table's pages live
  table.rs      a table: schema + heap, constraint checks
  types.rs      values, data types, columns
  result.rs     query results and table printing
  error.rs      shared error type
  storage/
    disk.rs     DiskManager: fixed-size pages in one file, free list
    page.rs     slotted page: many tuples in one page
    heap.rs     table heap: linked list of pages, RecordId
    tuple.rs    row <-> bytes (null bitmap + column values)
tests/sql.rs    end-to-end tests
```

## Storage

A database is a single file of 4 KB pages:

- **Page 0** is the file header: magic, format version, page size, page count,
  free list head, and the root page.
- **The catalog** is a table heap starting at the root page. Each row holds a
  table's `CREATE TABLE` statement plus its first and last page.
- **Each table** is a linked list of slotted pages. A row is identified by its
  `RecordId` (page, slot) and stored as a null bitmap followed by its columns.
- **Freed pages** (from `DROP TABLE`) go on a free list and are reused.

Every statement that changes data is synced to disk before it returns. There is
no buffer pool, WAL, or index yet: every page access is a disk read, a crash in
the middle of a statement may leave it half applied, and primary key checks scan
the whole table. A row must fit in one page (about 4 KB).
