# Sequoia

A minimal relational database written in Rust with zero dependencies.
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
  main.rs      interactive shell
  lib.rs       module list and public exports
  database.rs  entry point; runs SQL and persists to disk
  lexer.rs     SQL text -> tokens
  parser.rs    tokens -> AST (recursive descent)
  ast.rs       statement and expression types
  executor.rs  runs statements, evaluates expressions
  table.rs     table storage and constraint checks
  types.rs     values, data types, columns
  result.rs    query results and table printing
  error.rs     shared error type
tests/sql.rs   end-to-end tests
```

## Storage

Tables live in memory. When a file is given, the whole database is rewritten
after each change as a plain SQL script (`CREATE TABLE` + `INSERT`), and
opening the file replays that script. The file is human-readable.
