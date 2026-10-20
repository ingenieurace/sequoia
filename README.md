# Sequoia

A minimal disk-based relational database written in Rust with zero dependencies.
It is meant to be small and easy to read, not fast.

## Usage

```bash
cargo run                # in-memory database
cargo run -- my.db       # persisted to my.db (and its log, my.db-wal, while open)
```

```sql
CREATE TABLE users (id INT PRIMARY KEY, name TEXT, active BOOL);
INSERT INTO users VALUES (1, 'alice', TRUE), (2, 'bob', FALSE);
SELECT name FROM users WHERE active AND id > 0 ORDER BY name DESC LIMIT 10;
UPDATE users SET active = NOT active WHERE id = 2;
DELETE FROM users WHERE name = 'alice';
CREATE INDEX users_name ON users (name);
EXPLAIN SELECT * FROM users WHERE name = 'bob';  -- the plan, ending in SEARCH users USING INDEX users_name
DROP INDEX users_name;
DROP TABLE users;
```

Shell commands: `.tables`, `.exit`.

## Testing

```bash
cargo test
```

Besides unit and end-to-end tests, `tests/differential.rs` generates random
schemas, rows, queries, changes and transactions, runs them on Sequoia and on
SQLite (through the `sqlite3` command; skipped if there is none), and fails if
the two disagree, printing the case as a script for the shell. It runs 40
cases by default; run more, or other ones, with:

```bash
SEQUOIA_DIFF_CASES=2000 SEQUOIA_DIFF_SEED=7 cargo test --release --test differential -- --nocapture
```

`benches/joins.rs` times every join method on 200,000 rows joined with
50,000, mostly against another method on the same rows, checks that each
query's plan uses the join it means to time, and prints the fastest of five
runs of each. Name groups (`hash`, `index`, `block`, `merge`) to run only those:

```bash
cargo bench --bench joins
```

```bash
cargo bench --bench joins -- index merge
```

If there is a `sqlite3` command, the same tables are filled in SQLite and
every query is timed there as well (by the shell's `.timer`; SQLite counts
each query's rows instead of returning them), printed beside Sequoia's time
with the ratio, and the row counts are checked to agree. Pass `--no-sqlite`
to skip it:

```bash
cargo bench --bench joins -- --no-sqlite
```

`benches/sort.rs` times `ORDER BY` on 200,000 rows the same way: `memory`
with a budget every row fits in, `spill` with budgets from 4 MB down to
64 KB, so that more and more runs are written out and merged back, and `top`
with a `LIMIT`. Group names and `--no-sqlite` work as for the joins:

```bash
cargo bench --bench sort
```

```bash
cargo bench --bench sort -- spill
```

`benches/aggregate.rs` times aggregation on 200,000 rows the same way:
`groups` with from one group to one per row, by int, text, two and
expression keys, `functions` each aggregate over the same 100 groups,
`distinct` `COUNT(DISTINCT ...)` and `SELECT DISTINCT`, and `around` an
aggregate under `HAVING`, `ORDER BY` and `LIMIT`, or over a `WHERE`:

```bash
cargo bench --bench aggregate
```

```bash
cargo bench --bench aggregate -- distinct
```

`benches/scan.rs` times reading 200,000 rows, the cost under every other
step: `engines` the same rows in a heap, an LSM tree and a column store,
`filter` WHERE clauses keeping 1% to all of the rows, `project` more and
more columns and expressions (in Sequoia alone, as SQLite computes no column
its count leaves unread), and `limit` a `LIMIT` without `ORDER BY`.
`benches/subquery.rs` times subqueries over 200,000 orders: uncorrelated,
correlated with 10 to 10,000 distinct outer values, and in `FROM`.

`benches/lookup.rs` times finding rows by key, in batches of many queries,
each finding different rows, and prints how long each took: `point` one row
by a heap's indexes, an LSM tree's key, or reading every row, and `range`
0.1% to 10% of the rows by an index against reading them all. SQLite is
made to read each row found, not only its index.

`benches/writes.rs` times writes to a database file, made afresh and filled
for each of three runs: `insert` 50,000 rows into each storage engine and
with more indexes, `commit` single-row statements against one transaction,
`update` and `delete` a tenth of 50,000 rows or all of them, and
`checkpoint`. SQLite runs in WAL mode with `synchronous = FULL`, so it syncs
its log at each commit as Sequoia does.

Five benchmarks time the storage layer and its data structures alone, with
no SQL or SQLite: `benches/buffer_pool.rs` reads pages in five patterns
through 256 frames under each replacement policy and prints the hit rate and
time of each, `benches/lsm.rs` loads and reads LSM trees under level and
universal compaction and prints their write amplification,
`benches/column_encoding.rs` prints the encoding the column store picks for
each kind of data, the bytes a value takes, and the time to encode, decode
and filter it, and `benches/hash.rs` times the five hash tables of
`src/hash` (linear probing, cuckoo, chained, extendible and linear hashing)
against `std::collections::HashMap` on a million keys, and linear probing
alone at three load factors, and `benches/trie.rs` times the tries of
`src/trie` (trie, radix tree and ART) against a `BTreeMap` and the skip list
on INT-like and text keys, with the memory each takes, and loads LSM trees
with each kind of MemTable. Group names work for all of them:

```bash
cargo bench --bench writes -- commit
```

```bash
cargo bench --bench buffer_pool -- scans
```

## Supported SQL

- Statements: `CREATE TABLE ... [USING HEAP | LSM | COLUMN]`, `DROP TABLE`,
  `CREATE [UNIQUE] INDEX name ON table (key, ...) [INCLUDE (column, ...)]
  [WHERE condition]` (keys are columns or expressions of them, like
  `LOWER(name)`; `WHERE` makes a partial index), `DROP INDEX`, `INSERT`, `SELECT`,
  `UPDATE`, `DELETE`, `CHECKPOINT`, and `EXPLAIN` before a `SELECT`, `UPDATE` or
  `DELETE` to see its plan, or `EXPLAIN ANALYZE` before a `SELECT` to run it
  and see what each step of its plan did
- Transactions: `BEGIN`, `COMMIT`, `ROLLBACK`, and `SAVEPOINT name`,
  `ROLLBACK TO name`, `RELEASE name` (each may add `TRANSACTION` or `SAVEPOINT`
  as in SQL)
- Types: `INT`, `REAL` (or `FLOAT`: a 64-bit floating-point number, written
  `2.5` or `1e20`), `TEXT`, `BOOL`, and `NULL`. INTs and REALs compare, sort,
  group and join by value (`1 = 1.0`); arithmetic with a REAL gives a REAL, and
  a REAL that would be infinite is an error
- Constraints: `PRIMARY KEY` (unique, not null), unique indexes (keys with a
  NULL may repeat; a partial one checks only the rows it holds);
  values are type-checked, but an INT stored in a REAL column becomes a REAL,
  and a REAL with no fraction stored in an INT column an INT
- `FROM`: one table, or several joined: `[INNER] JOIN ... ON`, `LEFT [OUTER]
  JOIN ... ON`, `CROSS JOIN` or commas, with aliases (`users u`, `users AS u`)
  and qualified columns (`u.id`). A table may be a subquery with an alias,
  `(SELECT ...) d`, whose columns are named by its select list
- `SELECT [DISTINCT]`: `*` or expressions, each maybe named with `AS` (a name
  `ORDER BY` may use), `WHERE`, `GROUP BY` (expressions), `HAVING`,
  `ORDER BY ... ASC|DESC` (any expressions, NULL first, stable; with `DISTINCT`, only
  ones in the select list), `LIMIT n [OFFSET m]`
- `UNION` and `UNION ALL` between queries that return as many columns, named
  by the first; `UNION` drops repeated rows. An `ORDER BY`, `LIMIT` or `OFFSET`
  after the last query applies to all of the rows
- Aggregates: `COUNT(*)`, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, each also as
  `f(DISTINCT expr)`, which counts each distinct value once. They skip NULLs;
  over no values `COUNT` is 0 and the rest NULL. `SUM` is an INT over INTs and a
  REAL once a value is; `AVG` is a REAL. A selected column must be grouped or aggregated
- Expressions: `+ - * /` (`/` of two INTs truncates toward zero), `||` (joins
  TEXTs), `= != <> < <= > >=`, `AND OR NOT`, `IS [NOT] NULL`, `[NOT] IN (list)`,
  `[NOT] BETWEEN a AND b`, `[NOT] LIKE pattern` (`%` any characters, `_` one;
  ASCII letters match in either case, as in SQLite), `CASE [x] WHEN ... THEN ...
  [ELSE ...] END`, parentheses, with SQL three-valued logic for `NULL`
- Functions: `COALESCE(a, b, ...)`, `NULLIF(a, b)`, `LENGTH(text)`,
  `UPPER(text)` and `LOWER(text)` (ASCII letters), `SUBSTR(text, start[, length])`,
  `ABS(number)`, and `ROUND(number[, digits])`, which gives a REAL, halves away
  from zero. These, `LIKE` and `BETWEEN` work as in SQLite, but values are not
  converted between types: `LENGTH(5)` is an error. `CASE` and `COALESCE`
  evaluate only the parts they need
- Subqueries: `(SELECT ...)` as a value (one column, at most one row; NULL for
  none), `[NOT] EXISTS (SELECT ...)` and `expr [NOT] IN (SELECT ...)`, in the
  select list, `WHERE`, `HAVING`, `ORDER BY`, `UPDATE`, `DELETE` and `VALUES`.
  They may be correlated (refer to the columns of the query around them) and
  nested; each runs once per distinct set of outer values it is given

Every statement is atomic and, outside a transaction, durable once it returns.
A transaction is durable once `COMMIT` returns; until then a crash, `ROLLBACK`
or closing the database undoes all of it. A statement that fails inside one is
undone alone, and the transaction goes on. There is one connection, so no
concurrency control between transactions; below that, the buffer pool and
B+ trees may be used by many threads at once, with page latches. Not supported: `RIGHT` and `FULL` joins, subqueries in
`JOIN ... ON`, `LATERAL`, `INSERT ... SELECT`.

## Layout

Each layer uses only the ones below it:

```
sql -> executor -> plan -> catalog -> storage        (types, predicate, error: shared by all)
```

```
src/
  main.rs         interactive shell
  lib.rs          module list and public exports
  database.rs     entry point; runs SQL, commits or rolls back each statement or transaction
  executor/       runs statements
    mod.rs        statement execution; a SELECT's rows as a stream (`Database::query`)
    operator.rs   each plan step as an iterator pulling rows from the one below
    join.rs       block nested loop, hash, index and sort-merge joins; LEFT JOIN padding
    sort.rs       ORDER BY: external merge sort, top-N heap for LIMIT
    aggregate.rs  hash aggregation: one row per group, and the aggregate functions
    subquery.rs   runs subqueries for each row, with outer references replaced by its values
  types.rs        values, data types, columns
  predicate.rs    `column op literal` conditions, shared by the planner and storage
  result.rs       query results and table printing
  error.rs        shared error type
  sql/            the language
    lexer.rs      SQL text -> tokens
    parser.rs     tokens -> AST (recursive descent)
    ast.rs        statement and expression types
    eval.rs       evaluates expressions, with three-valued logic for NULL
  plan/           how to read a table
    filter.rs     WHERE -> pushed-down predicates; the columns a statement reads
    access.rs     picks a full scan, an index or a key lookup
    query.rs      a SELECT as a tree of steps: binds names, pushes filters down, picks joins
  catalog/        tables and indexes
    mod.rs        Catalog: table and index schemas and where their pages live
    table.rs      a table: schema + storage + indexes, constraint checks
    index.rs      secondary index: B+ tree entries of (keys, included columns, row id)
  storage/
    io/           durable pages
      medium.rs   byte-addressed files: on disk, in memory, or crashing (for tests)
      wal.rs      write-ahead log: page frames, commits, savepoints, recovery, checkpoints
      disk.rs     DiskManager: fixed-size pages in one file, free list, via the WAL
    buffer/       pages cached in memory
      pool.rs     buffer pool: cached frames, pin counts, dirty pages
      replacer.rs page replacement policies: ARC (default), LRU-K, CLOCK
    page.rs       slotted page: many tuples in one page
    tuple.rs      row <-> bytes (null bitmap + column values)
    sort_key.rs   values <-> bytes that sort like them: sort keys and index keys
    heap.rs       table heap: linked list of pages, free-space map, RecordId
    btree.rs      B+ tree: sorted byte-string keys, splits, merges, range scans, latch crabbing
    lsm/          log-structured merge tree
      tree.rs     log + MemTable, SSTables, level/universal compaction
      memtable.rs the MemTable: a skip list or a trie, as `MEMTABLE` says
      skiplist.rs skip list: a MemTable, nodes in a Vec linked by index
      sstable.rs  SSTable: sorted entries in pages, in-memory summary
      bloom.rs    Bloom filter for SSTable lookups
    column/       column store
      store.rs    PAX row groups, delete bitmaps, delta store
      encoding.rs column compression: RLE, bit-packing, patching, delta, dictionary, bitmap
  hash/           hash tables to compare with std's HashMap (nothing uses them yet)
    linear_probe.rs, cuckoo.rs       static: one array of slots
    chained.rs, extendible.rs, linear_hash.rs   dynamic: buckets that split
  trie/           tries: sorted maps that branch on a byte of the key per level
    mod.rs        one tree for the trie, radix tree (paths compressed) and ART
    list.rs       children in a sorted list: the trie's and radix tree's
    adaptive.rs   ART's children: Node4, Node16, Node48, Node256
tests/sql.rs      end-to-end tests
tests/differential.rs  random statements run on Sequoia and SQLite, which must agree
```

## Storage

A database is a single file of 4 KB pages:

- **Page 0** is the file header: magic, format version, page size, page count,
  free list head, and the root page.
- **The catalog** is a table heap starting at the root page. Each row holds a
  table's `CREATE TABLE` statement plus its first and last page, or an index's
  `CREATE INDEX` statement plus its root page. (An index from a file written
  before indexes had several keys is built again when the file is opened.)
- **A heap table** (the default) is a linked list of slotted pages. A row is identified by its
  `RecordId` (page, slot) and stored as a null bitmap followed by its columns.
  An in-memory free-space map (rebuilt from the pages after opening) sends
  each insert to the first page with room, so space freed by deletes is
  reused. An update keeps a row in its page when the page has room for the
  new value, and only otherwise deletes it and inserts it again.
- **An LSM table** (`USING LSM`) is a log-structured merge tree keyed by the
  primary key, or by a hidden rowid if there is none. Writes, deletes
  included, go to an in-memory MemTable, and are appended to a log. The
  MemTable is an adaptive radix tree (default), or a radix tree, a trie or a
  skip list: to switch, edit `MEMTABLE` in `src/storage/lsm/memtable.rs`. When the
  log reaches 64 KB the MemTable is written out sorted as an SSTable.
  SSTables are merged by **level compaction** (default): four level-0
  SSTables are merged into level 1, and each deeper level is a single SSTable
  up to 10 times bigger than the one above, merged down when it outgrows
  that. **Universal compaction** keeps every SSTable in one level instead;
  once there are 8, it merges runs of similar size, or all of them when the
  newer ones take twice the space of the oldest. It rewrites data less often
  but leaves more SSTables to check. To switch, edit `COMPACTION` in
  `src/storage/lsm/tree.rs`. Lookups check the MemTable, then SSTables newest first,
  skipping those whose key range or Bloom filter rules the key out. Scans
  merge the MemTable and the SSTables in key order, each read in place from
  the key the scan starts at, so a range of keys reads only its own pages. A
  manifest page lists the log and the SSTables. SSTable summaries are not
  stored: opening a table reads its SSTables once to rebuild them.
- **A column table** (`USING COLUMN`) is a column store in the PAX layout.
  New rows go to a **delta store**, a heap of whole rows. Once it holds 1024
  rows they are migrated into a **row group**, where each column's values are
  stored together in their own chunk, so a query reads only the columns it
  uses. Each chunk is compressed with whichever lossless encoding makes it
  smallest: run-length, bit-packing (relative to the smallest value),
  patching (bit-packing with a few outliers stored apart), delta (then
  encoded again), an order-preserving dictionary, or one bitmap per value.
  A WHERE clause's conditions of the form `column op literal` (joined by
  AND) are **pushed down** into the scan and evaluated on the compressed
  chunks: once per run for run-length encoding, by picking the literal's
  bitmap, or, for a dictionary, by binary searching the sorted dictionary
  for the literal and comparing codes instead of values. Only the rows that
  match are then decoded from the other columns' chunks (late
  materialization); the full WHERE clause is still checked on them.
  Row groups are never rewritten: a delete sets the row's bit in the group's
  delete bitmap, an update deletes the row and inserts the new value into the
  delta store, and a group whose rows are all deleted is freed. A manifest
  page lists the delta store and the row groups, which limits a column table
  to about a thousand row groups.
- **An index** is a B+ tree over the keys of a heap or LSM table's rows:
  columns, or expressions of them. Each entry is a key of the tree: the keys,
  each encoded as a sort key is (so bytes sort like values, NULL first, and
  each value's encoding ends where it can be told to), then the `INCLUDE`
  columns, then the row's id (a heap row's page and slot, or an LSM row's
  key). The entries of rows with equal first keys are therefore adjacent, in
  the order of the next key, and a lookup is a range scan of the leaves,
  which are linked in key order. A partial index holds only the rows its
  `WHERE` is true for; every other row has an entry, NULL keys included. A
  node that overflows its
  page splits; a delete that leaves a node under a quarter full merges it
  with a neighbour, or moves keys over from it (and a parent that the longer
  separator this may bring overflows splits in turn). The root page never
  moves, so the catalog row stays valid. Threads may search and change a
  tree at once, latching pages by **latch crabbing**: down from the root,
  a node's latch is released only once its child's is held. A change that
  fits in its leaf write-latches only the leaf; one that must split or merge
  write-latches the way down, releasing everything above each node that the
  change cannot spread past. A scan copies one leaf at a time and, instead
  of following the leaf links (a merge may free the next leaf), goes down
  again to the key that bounds the leaf above, which it noted on the way. Every insert, update and delete updates every
  index of the table. A heap table with a primary key gets a unique index
  `<table>_pkey` on it, which enforces the key and is dropped only with the
  table. Column tables cannot have indexes: migrating rows into a row group
  moves them, which would change their ids.
- **Finding rows**: a WHERE clause's conditions of the form `expression op
  literal` (joined by AND) choose how a statement reads its table. A heap or
  LSM table uses the index those conditions narrow down most: equalities on
  its first keys, then a range on the next key (an equality counts more than
  a range with two ends, which counts more than one with one end). A partial
  index serves only a WHERE that implies its condition: each of the
  condition's conjuncts is one of the WHERE's, or follows from one of its
  comparisons (`x > 5` gives `x > 0`), or is `e IS NOT NULL` for an `e` it
  compares. The scan reads only the rows the index lists, in index order;
  when the index's column keys and included columns hold every column the
  statement reads, it reads them from the index alone (`USING COVERING
  INDEX`). An LSM table whose primary key is fixed by an
  equality looks that key up, and one whose primary key the conditions bound
  reads only that range of keys, in key order (ranked as an index would be,
  and winning ties). Otherwise every row is read, a column table's
  with its conditions pushed down (above). The full WHERE clause is always
  checked on the rows read. `EXPLAIN` shows the choice on its last line.
- **Freed pages** (from `DROP TABLE`, `DROP INDEX`, B+ tree merges, an LSM
  flush or compaction, or a column store migration) go on a free list and
  are reused. A rolled-back statement's allocations are undone with it.

Pages are read and written through a **buffer pool** of 256 frames (1 MB). A
page in use is pinned and cannot be evicted; when the pool is full, ARC
(Adaptive Replacement Cache) picks an unpinned victim, writing it back first if
it is dirty. ARC balances recently used against frequently used pages, so a
full table scan does not push frequently used pages out. LRU-K and CLOCK are
also available: to switch, edit `REPLACEMENT_POLICY` in `src/storage/buffer/pool.rs`.

**Plans**: a `SELECT` is planned as a tree of steps, each reading rows from
the one below it, and run in the Volcano style: each step is an iterator
that pulls rows from its input. For example:

```
EXPLAIN SELECT city, COUNT(*) FROM users WHERE active GROUP BY city HAVING COUNT(*) > 1 ORDER BY city LIMIT 5;

PROJECT city, COUNT(*)
  SORT BY city (TOP 5)
    FILTER COUNT(*) > 1
      AGGREGATE COUNT(*) BY city
        FILTER active
          SCAN users
```

Scans, filters, limits and projections hand rows on one at a time, so a
`LIMIT` without `ORDER BY` stops the scan once it has enough, and
`Database::query` returns a `SELECT`'s rows as they are made instead of
collecting them (`execute` collects them). Sorts and aggregates need all of
their input before their first row.

`EXPLAIN ANALYZE` runs a `SELECT`, reading all of its rows, and shows each
step of its plan with the rows it returned and the time it took, the time of
the steps below it included, and a last line with the query's rows and time:

```
EXPLAIN ANALYZE SELECT g, COUNT(*) FROM a WHERE k < 5000 GROUP BY g ORDER BY g;

PROJECT g, COUNT(*)  (10 rows, 4.0 ms)
  SORT BY g  (10 rows, 4.0 ms)
    AGGREGATE COUNT(*) BY g  (10 rows, 4.0 ms)
      FILTER k < 5000  (9993 rows, 2.9 ms)
        SCAN a  (20000 rows, 1.4 ms)
10 rows in 4.0 ms
```

A step that runs more than once, as the right side of a block nested loop
join does for each block of left rows, says how often, with its rows counted
over all of its runs; an index join's lookups are counted with the join. Each
step's time includes timing it, about the cost of reading the clock twice for
each row it returns.

**Joins**: the tables of `FROM` are joined left to right. Every column
reference is first bound to its table; an unqualified name that several
tables have is an error. `WHERE`, and the `ON` of inner joins, are split at
their `AND`s: a condition on one table is pushed down to that table's scan,
where it may pick an index, and one on several tables goes to the join that
brings in the last of them. A `LEFT JOIN` keeps every left row, so only the
parts of its `ON` about the right table alone are pushed down, and `WHERE`
conditions on its right table are checked after it, on the NULL-padded rows
too. Each join then finds the right rows that may match a left row from its
equalities `left = right column`: an **index join** looks them up in an
index of the right table on that column (or an LSM primary key), once per
left row, when that is cheaper than a hash join; otherwise a **hash join**
reads the right table once into a hash table keyed by the equalities;
failing any equality, a **block nested loop join** reads the left rows a
block at a time, as many as fit in its budget (4 MB by default;
`Database::set_join_memory` changes it), and reads the right table again
for each block, unless its rows fit in the budget too and are kept from the
first read, trying every pair. The rest of the condition is checked on each
pair. Keys are compared as values, so a NULL key matches nothing, and
neither do keys of different types, where `=` would report an error. The
right side of a hash join is held in memory; a block nested loop join holds
one block of left rows, and the right rows if it keeps them.

Whether lookups are cheaper depends on how many there are against how many
rows a hash join would read. The planner counts each table's rows the first
time it needs them (writes keep the count up to date after that; a rollback
makes it count again), and with no statistics guesses what share of them a
condition keeps as PostgreSQL does without any: 1 in 200 for an equality,
a third for a comparison. A lookup costs about 6 rows read when it goes
through an index, and 2 for an LSM primary key; a hash join about 2 per right
row and 0.4 per left row (measured with `benches/joins.rs`). So an index
join over 200,000 rows into 50,000 becomes a hash join, about 3 times faster,
while one of up to about a third as many rows as the right side stays.

A **sort-merge join** sorts both sides by the equalities, with the same
external sort as `ORDER BY` (and its budget), and walks them together; its
rows come out in key order. There are no statistics to weigh it against a
hash join, so it is picked only when that order saves a sort: when `ORDER BY`
starts with a join's keys, follows them only with columns of the tables
before it, and every later join is a hash or index join, which keep their
left side's order. That join becomes a merge join and the `ORDER BY` needs
no sort of its own (`EXPLAIN` shows `MERGE JOIN ... (SORTED BY ...)` and no
`SORT BY`).

**Grouping**: a `SELECT` with `GROUP BY`, `HAVING` or an aggregate gets an
aggregate step, which works by hash aggregation. Each expression of its
select list, `HAVING` and `ORDER BY` is first rewritten to read the
aggregate's rows: a `GROUP BY` expression and each distinct aggregate become
columns named by their SQL text (`city`, `COUNT(*)`), and any column left
over is an error, since it is neither grouped nor aggregated. The step folds
each row into its group's accumulators, in a hash table with one entry per
group, held in memory. Groups come out in the order their first row was
read; without `GROUP BY` all rows form one group, even when there are none.

**Sorting**: `ORDER BY` buffers rows in memory up to a budget (4 MB by
default; `Database::set_sort_memory` changes it). Past that, it sorts what it
holds and writes it to a temporary file as a sorted run, and at the end
merges the runs, with the last rows still in memory, through a tournament
tree. A merge reads 16 runs at once, or one per 64 KB of the budget (up to
256); if there are more, the smallest are merged first, as few as it takes
for every later pass to merge a full set. Sort keys are encoded as bytes
that compare as the values do (NULL first, then each type's values apart),
so comparing rows compares bytes, and a key of up to eight bytes, as most
INT keys are, compares as one number. The temporary file is removed as soon
as it is opened, so it vanishes with the sort, and it bypasses the
write-ahead log. With `LIMIT n`, the buffer is instead a heap of the best `n`
rows so far, so sorting `m` rows takes O(m log n) time and memory for `n`
rows, and once a run of `n` rows is written, later rows that sort after its
last are dropped as they arrive. Rows with equal keys keep the order they
were read in.

**The write-ahead log** (`<file>-wal`) makes every statement atomic. The
DiskManager never writes the database file directly: each page write, the
header page's included, is appended to the log as a frame holding the whole
page, and reads find a page's newest frame through an in-memory map. When a
statement that changes data succeeds, its dirty pages are written back from
the buffer pool and a commit frame is appended and synced: one sync per
statement. When it fails, it is rolled back: the log is cut back to the last
commit frame, the buffer pool drops its cached pages, and the catalog is
opened again from the committed pages. Each frame carries a checksum of
itself and the frame before it, so opening the log after a crash keeps the
frames up to the last valid commit frame and cuts off the rest: a statement
is either all there or not at all. A **checkpoint** copies the newest
committed version of each page into the database file, syncs it, and only
then empties the log, so a crash during one leaves the log to be copied
again. It runs after a commit that leaves the log at 4 MB or more, on
`CHECKPOINT`, when the database is closed (which also removes the log), and
when it is opened after a crash. Crash tests run every kind of statement on
simulated files that stop at an arbitrary write, and check that every table
and index afterwards shows the statement entirely or not at all.

Primary key checks use the key's index on heap tables, look the key up on LSM
tables and read the key column on column tables (heap tables in files written
before indexes existed have no key index and scan the table). A row must fit
in one page (about 4 KB), and an index entry in 1 KB.
