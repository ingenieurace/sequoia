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

- Statements: `CREATE TABLE ... [USING HEAP | LSM]`, `DROP TABLE`, `INSERT`,
  `SELECT`, `UPDATE`, `DELETE`
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
    buffer.rs   buffer pool: cached frames, pin counts, dirty pages
    replacer.rs page replacement policies: ARC (default), LRU-K, CLOCK
    page.rs     slotted page: many tuples in one page
    heap.rs     table heap: linked list of pages, free-space map, RecordId
    lsm.rs      LSM tree: log + MemTable, SSTables, level/universal compaction
    sstable.rs  SSTable: sorted entries in pages, in-memory summary
    bloom.rs    Bloom filter for SSTable lookups
    tuple.rs    row <-> bytes (null bitmap + column values)
tests/sql.rs    end-to-end tests
```

## Storage

A database is a single file of 4 KB pages:

- **Page 0** is the file header: magic, format version, page size, page count,
  free list head, and the root page.
- **The catalog** is a table heap starting at the root page. Each row holds a
  table's `CREATE TABLE` statement plus its first and last page.
- **A heap table** (the default) is a linked list of slotted pages. A row is identified by its
  `RecordId` (page, slot) and stored as a null bitmap followed by its columns.
  An in-memory free-space map (rebuilt from the pages after opening) sends
  each insert to the first page with room, so space freed by deletes is
  reused. An update keeps a row in its page when the page has room for the
  new value, and only otherwise deletes it and inserts it again.
- **An LSM table** (`USING LSM`) is a log-structured merge tree keyed by the
  primary key, or by a hidden rowid if there is none. Writes, deletes
  included, go to an in-memory MemTable and are appended to a log. When the
  log reaches 64 KB the MemTable is written out sorted as an SSTable.
  SSTables are merged by **level compaction** (default): four level-0
  SSTables are merged into level 1, and each deeper level is a single SSTable
  up to 10 times bigger than the one above, merged down when it outgrows
  that. **Universal compaction** keeps every SSTable in one level instead;
  once there are 8, it merges runs of similar size, or all of them when the
  newer ones take twice the space of the oldest. It rewrites data less often
  but leaves more SSTables to check. To switch, edit `COMPACTION` in
  `src/storage/lsm.rs`. Lookups check the MemTable, then SSTables newest first,
  skipping those whose key range or Bloom filter rules the key out. A
  manifest page lists the log and the SSTables. SSTable summaries are not
  stored: opening a table reads its SSTables once to rebuild them.
- **Freed pages** (from `DROP TABLE`, an LSM flush or compaction) go on a free
  list and are reused.

Pages are read and written through a **buffer pool** of 256 frames (1 MB). A
page in use is pinned and cannot be evicted; when the pool is full, ARC
(Adaptive Replacement Cache) picks an unpinned victim, writing it back first if
it is dirty. ARC balances recently used against frequently used pages, so a
full table scan does not push frequently used pages out. LRU-K and CLOCK are
also available: to switch, edit `REPLACEMENT_POLICY` in `src/storage/buffer.rs`.

Every statement that changes data has its dirty pages written back and synced
before it returns. There is no WAL for the whole database or index yet: a
crash in the middle of a statement may leave it half applied, and primary key
checks on heap tables scan the whole table (LSM tables look the key up). A
row must fit in one page (about 4 KB).
