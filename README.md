# Sequoia

A relational database written from scratch in Rust, with no dependencies
beyond the standard library.

Sequoia is a study of how databases work. It implements the textbook
techniques side by side, so they can be read and compared on the same data:
three storage engines, two kinds of index, three execution models, five join
methods, three page replacement policies, and intra-query parallelism. It is
also a working database: SQL, transactions under multiversion concurrency
control (snapshot isolation), crash recovery through a write-ahead log, a
shell, and a TCP server.

Every source file starts with a doc comment that explains its design, often
with a diagram. Reading the code is the intended way to learn it; this README
is the map.

## Contents

- [Quick start](#quick-start)
- [What is inside](#what-is-inside)
- [Shell, server and library](#shell-server-and-library)
- [SQL](#sql)
- [Source layout](#source-layout)
- [How it works](#how-it-works)
- [Settings and switches](#settings-and-switches)
- [Limits](#limits)
- [Testing](#testing)
- [Benchmarks](#benchmarks)

## Quick start

Sequoia needs a recent stable Rust toolchain: it uses edition 2024 and
`File::try_lock`, which is stable since Rust 1.89.

```bash
cargo run --release                 # a database in memory
cargo run --release -- my.db        # a database in my.db (its log is my.db-wal while open)
```

```sql
CREATE TABLE users (id INT PRIMARY KEY, name TEXT, city TEXT, active BOOL);
CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total REAL);
CREATE INDEX orders_user ON orders (user_id);
INSERT INTO users VALUES (1, 'ann', 'oslo', TRUE), (2, 'bob', 'rome', TRUE), (3, 'cy', 'oslo', FALSE);
INSERT INTO orders VALUES (1, 1, 9.5), (2, 1, 20.0), (3, 2, 5.25);

SELECT u.city, COUNT(*), SUM(o.total)
FROM users u JOIN orders o ON o.user_id = u.id
WHERE u.active GROUP BY u.city ORDER BY u.city;

EXPLAIN SELECT u.city, COUNT(*), SUM(o.total)
FROM users u JOIN orders o ON o.user_id = u.id
WHERE u.active GROUP BY u.city HAVING COUNT(*) > 1 ORDER BY u.city LIMIT 5;
```

The `EXPLAIN` prints the plan, one step per line, each step indented under
the step that reads its rows:

```
PROJECT u.city, COUNT(*), SUM(o.total)
  SORT BY u.city (TOP 5)
    FILTER COUNT(*) > 1
      AGGREGATE COUNT(*), SUM(o.total) BY u.city
        INDEX JOIN
          FILTER u.active
            SCAN users u
          LOOKUP orders o USING INDEX orders_user (o.user_id = u.id)
```

`EXPLAIN ANALYZE` runs the query and adds the rows and time of each step, the
rows the planner estimated it would return, and the execution model the
query ran in, with the reason it was chosen. `ANALYZE` gathers statistics
that make those estimates closer, and let the planner reorder joins.

## What is inside

| Area | What Sequoia has |
|---|---|
| Storage engines | heap (slotted pages), LSM tree, column store (PAX row groups with compression) |
| Indexes | B+ tree with latch crabbing; hash index with extendible or linear hashing; multi-column, expression, partial (`WHERE`), covering (`INCLUDE`) and unique indexes |
| Column compression | run-length, bit-packing, patched bit-packing, delta, order-preserving dictionary, bitmap; filters run on compressed data; zone maps |
| LSM tree | MemTable as a skip list, trie, radix tree or adaptive radix tree (ART); level or universal compaction; Bloom filters |
| Buffer pool | 256 frames in 8 shards; ARC, LRU-K or CLOCK replacement, switchable at runtime |
| Durability | write-ahead log of full pages with chained checksums; atomic statements; transactions and savepoints; checkpoints |
| Execution models | iterator (Volcano), materialization, vectorized (batches of 1024 rows, a column at a time, SIMD-friendly loops) |
| Joins | hash join (hybrid, spills to disk), index join, sort-merge join, block nested loop join, naive nested loop join (for comparison only); inner, `LEFT`, `RIGHT`, `FULL` and `LATERAL` |
| Queries | subqueries anywhere a value goes; `UNION`, `INTERSECT`, `EXCEPT`; common table expressions, recursive too; window functions over partitions and frames |
| Parallelism | parallel heap scans, parallel grace hash join, gathered hash and index joins, two-phase (partial and final) aggregation |
| Sort and aggregation | external merge sort with a tournament tree, top-N heap for `LIMIT`; hash aggregation that spills partitions |
| Planner | access path choice, predicate pushdown, cost-based choice between index and hash joins, merge join to save a sort, join reordering by dynamic programming |
| Statistics | `ANALYZE`: reservoir sample, null share, distinct count (Haas-Stokes), most common values, equi-depth histograms; kept in the catalog |
| Concurrency | multiversion concurrency control: snapshot isolation, row versions with xmin and xmax in every storage engine, a commit log, first updater wins, waits for row conflicts with deadlock detection, `VACUUM` and autovacuum; a thread per connection in the server |

## Shell, server and library

### Shell

```bash
sequoia [FILE] [--replacer arc|lru|lru-K|clock]
```

Statements end with `;` and may span lines. The prompt is `sequoia*>` while a
transaction is open.

On a terminal (on Unix), lines can be edited: Up and Down bring back the
lines typed before, Left, Right, Home, End, Backspace and Delete edit the
line, Ctrl-U and Ctrl-K delete before or after the cursor, Ctrl-C drops the
statement being typed, and Ctrl-D on an empty line leaves the shell. The
history lasts until the shell exits. Input from a pipe, and the shell on
Windows, read whole lines without editing.

Commands start with `.`:

| Command | Effect |
|---|---|
| `.tables` | lists the tables |
| `.model [auto \| iterator \| materialization \| vectorized]` | shows, or sets, the execution model of every query; `auto` lets each query pick |
| `.workers [N]` | shows, or sets, how many threads scans and hash joins use (1 by default) |
| `.replacer [arc \| lru \| lru-K \| clock]` | shows the replacement policy and its hit rate since it was set, or switches policy while keeping the cached pages |
| `.exit`, `.quit` | leaves the shell |

### Server

```bash
sequoia [FILE] --serve [ADDR] [--max-connections N]   # serve a database over TCP
sequoia --connect [ADDR]                               # open a shell on a server
```

`ADDR` is `host:port`, a port alone (on 127.0.0.1), or with `=` a host alone
(`--serve=0.0.0.0`, on port 1024). By default it is `127.0.0.1:1024`, with at
most 64 connections. Each connection is a thread with its own session (its
own settings and transaction). The protocol is lines of text, so `nc` works
as a client: the client sends what the shell would read, and each answer ends
with a line `.end`, or `.end*` inside a transaction. The server accepts
`.tables`, `.model`, `.workers` and `.exit`; the replacement policy is chosen
with `--replacer` when the server starts.

### Library

```rust
use sequoia::{Database, SharedDatabase};

let mut db = Database::open("my.db")?;          // or Database::in_memory()
db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")?;
db.execute("INSERT INTO t VALUES (1, 'ann'), (2, 'bob')")?;

// Rows as they are made, without collecting them.
for row in db.query("SELECT name FROM t ORDER BY id")? {
    println!("{:?}", row?);
}

// Many threads: one session each.
let shared = SharedDatabase::in_memory();
let mut session = shared.session();        // one per thread; clone `shared` into each
session.execute("CREATE TABLE t (id INT PRIMARY KEY)")?;
```

A `Database` is for one thread. A `SharedDatabase` is shared by many, each
through a `Session` with its own settings, transaction and busy timeout.
Both have setters for the memory budgets, the execution model, the number of
workers, and a join method to force on every join (see
[Settings and switches](#settings-and-switches)).

## SQL

**Statements**

- `CREATE TABLE name (column TYPE [constraint ...], ... [, table constraint, ...]) [USING HEAP | LSM | COLUMN]`, `DROP TABLE name`
  (see Constraints below)
- `CREATE [UNIQUE] INDEX name ON table [USING BTREE | HASH] (key, ...) [INCLUDE (column, ...)] [WHERE condition]`, `DROP INDEX name`.
  A key is a column or an expression of columns, such as `LOWER(name)`.
- `INSERT INTO table [(column, ...)] VALUES (...), ...`, `INSERT INTO table [(column, ...)] SELECT ...`
  (the query's rows are all read before the first is inserted), or `INSERT INTO table DEFAULT VALUES`;
  a value in `VALUES` may be `DEFAULT`
- `SELECT`, `UPDATE table SET column = expr | DEFAULT, ... [WHERE ...]`, `DELETE FROM table [WHERE ...]`
- `RETURNING * | expr [AS name], ...` after `INSERT`, `UPDATE` or `DELETE` returns the rows written instead of a count
- `EXPLAIN` before `SELECT`, `UPDATE` or `DELETE`; `EXPLAIN ANALYZE` before `SELECT`
- `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name`, `RELEASE [SAVEPOINT] name` (each may add `TRANSACTION`)
- `CHECKPOINT` copies the committed pages from the log into the database file
- `ANALYZE [table]` computes the statistics of a table, or of every table, for the planner (see [Planning](#planning))
- `VACUUM [table]` removes the row versions of a table, or of every table, that no transaction can see any more (see [Transactions and MVCC](#transactions-and-mvcc))

**Types**: `INT` (or `INTEGER`, 64-bit), `REAL` (or `FLOAT`, 64-bit float, never
NaN or infinite), `TEXT` (UTF-8), `BOOL` (or `BOOLEAN`), and `NULL`. INTs and
REALs compare, group and join by value (`1 = 1.0`). Values are type-checked,
with no implicit conversion between other types: `LENGTH(5)` is an error.
Also:

| Type | Literal | Holds |
|---|---|---|
| `DATE` | `DATE '2024-02-29'` | a day from year 1 to 9999 |
| `TIME` | `TIME '13:45:07.25'` | a time of day, to the microsecond |
| `TIMESTAMP` | `TIMESTAMP '2024-02-29 13:45:07'` | both, with no time zone |
| `TIMESTAMPTZ` (or `TIMESTAMP WITH TIME ZONE`) | `TIMESTAMPTZ '2024-02-29 13:45:07+02'` | an instant, kept in UTC |
| `INTERVAL` | `INTERVAL '1 year 2 mons 3 days 04:05:06'`, `INTERVAL '3' DAY` | a span of months, days and microseconds |
| `BLOB` | `X'CAFE00'` | bytes, compared byte by byte |
| `DECIMAL(p, s)` (or `NUMERIC`) | `DECIMAL '19.99'` | exact numbers of up to `p` ≤ 38 digits, `s` after the point |

A DECIMAL column rounds what is stored in it to its `s` places (halves away
from zero) and refuses a value with more than `p - s` digits before the
point; a plain `DECIMAL` keeps any value of up to 38 digits, without the
zeros at the end of its fraction. An INT or a REAL stored in one is
converted, a REAL by the digits it prints with (`19.99`, not the binary
fraction near it); literals like `1.5` are REALs, so `price + 0.5` is a
REAL, and `price + DECIMAL '0.5'` a DECIMAL. DECIMALs add, subtract and
multiply exactly, and divide to six more places than their operands have;
with a REAL they become REALs. They compare, group and join with INTs and
REALs by value, and `SUM` and `AVG` of them are DECIMALs, exact, in
parallel too. A date plus or minus an INT is a date that many days on, two
dates' difference an INT, and a date plus a time a timestamp. The type
names are not keywords, so they stay free as names of columns.

An INTERVAL keeps its months, days and microseconds apart, as PostgreSQL
does, since a month is not always as many days: a timestamp plus `1 mon`
is the same day of the next month (or its last day), plus `30 days` 30
days on. Intervals add and subtract part by part, and multiply and divide
by numbers, a part's fraction passing down (a month as 30 days, a day as 24
hours); a timestamp, a TIMESTAMPTZ, a date (which makes a timestamp) or a
time moves by one; two timestamps' or times' difference is one (`2 days
02:00:00`). They read the way PostgreSQL writes them (`90 minutes`, `1.5
hours`, `-2 weeks`, `04:05:06`, `2 hours ago`), and print that way. They
order by their length, a month as 30 days, but are equal only if all three
parts are (where PostgreSQL has `1 mon` equal `30 days`), which keeps
equal intervals' index keys equal.

A TIMESTAMPTZ is an instant: a zone written with it (`Z`, `UTC`, `+02`,
`-05:30`) says how to read it, and is not kept. The session's time zone is
always UTC, so it prints in UTC (`2024-02-29 11:45:07+00`) and converts to
and from a TIMESTAMP as UTC. `x AT TIME ZONE zone` gives a TIMESTAMPTZ's
wall time in another zone, as a TIMESTAMP, and a TIMESTAMP's instant read
as a wall time there; a zone is UTC, an offset east of it (`'+05:30'`), or
an INTERVAL of one. There are no zones by name, and so no daylight saving
time: there is no time zone database.

**Constraints**: after a column's type, in any order:

- `PRIMARY KEY`: one column only (unique, not NULL)
- `NOT NULL` (or `NULL`, which is the default anyway)
- `DEFAULT expr`: what the column takes when an INSERT leaves it out or gives
  `DEFAULT`, or an UPDATE sets it to `DEFAULT`. An expression of no column
  and no subquery, which may read the current time (`DEFAULT CURRENT_DATE`),
  taken when the statement starts, the same for all its rows. It is checked
  against the column's type when the table is created. Unparenthesized, it is
  read up to the next constraint (`DEFAULT 0 NOT NULL`); `DEFAULT (a = b)`
  takes any expression.
- `UNIQUE`: no two rows share the value, but NULLs may repeat. A unique
  index named `<table>_<column>_key` keeps it, and cannot be dropped alone.
  Not on COLUMN tables, which have no indexes.
- `CHECK (condition)`: a row may not make it false (NULL passes). It reads
  only the row: no subquery, aggregate or current time.

Any of them may be named with `CONSTRAINT name` before it. Among the
columns, a table may also have `[CONSTRAINT name] CHECK (condition)`,
`UNIQUE (column, ...)` and `PRIMARY KEY (column)`. An unnamed CHECK is named
`<table>_<column>_check`, or `<table>_check`, as in PostgreSQL, with a
number after if that is taken; errors name it: `new row violates check
constraint item_price_check`. Every row an INSERT or UPDATE writes is
checked before any is written, so a statement that breaks a constraint
changes nothing. Unique indexes also stand on their own (rows with a NULL
key may repeat; a partial unique index checks only the rows it holds).

**Queries**

- `[WITH ...] SELECT [DISTINCT] * | expr [AS name], ... [FROM ...]`. Without
  `FROM`, a query reads one row of no columns: `SELECT 1 + 1`.
- `FROM` takes tables with aliases, `[INNER] JOIN ... ON`,
  `{LEFT | RIGHT | FULL} [OUTER] JOIN ... ON`, `CROSS JOIN`, commas, and
  subqueries with an alias, which may rename their columns: `(SELECT ...) d (a, b)`.
  `LATERAL (SELECT ...) d`, after a comma, `CROSS JOIN`, `JOIN` or `LEFT
  JOIN`, may read the columns of the tables before it, and runs for each of
  their rows.
- `JOIN ... USING (column, ...)` joins on those columns being equal on both
  sides, and `NATURAL JOIN` on all the columns the two sides share by name
  (of any kind of join). Each pair becomes one column: unqualified, its name
  reads the left side's, the right side's for a `RIGHT JOIN`, and the first
  that is not NULL for a `FULL JOIN`; `SELECT *` has it once, first, then
  the rest of each side's columns.
- `VALUES (expr, ...), ...` is a query of those rows, of columns `column1`,
  `column2`...: alone, in a set operation, in `IN (...)`, or in `FROM` as
  `(VALUES ...) t (a, b)`, `LATERAL` too.
- `WHERE`, `GROUP BY` (expressions), `HAVING`, `ORDER BY expr [ASC | DESC], ...`
  (NULL first, stable; may name a select-list alias), `LIMIT n [OFFSET m]`
- `UNION`, `INTERSECT` and `EXCEPT`, each also with `ALL`. `INTERSECT` binds
  tighter than the others, which go left to right, as in standard SQL
  (SQLite gives them all the same precedence). Rows equal in every column,
  NULLs included, are the same row. An `ORDER BY`, `LIMIT` or `OFFSET` after
  the last query applies to all rows.
- `WITH name [(column, ...)] AS (query), ...` names queries for the
  statement: each is read like a derived table wherever `FROM` names it, and
  hides a table of the same name. `WITH RECURSIVE` lets one read itself:
  `anchor UNION [ALL] step`, where only `step` names it, once, in its `FROM`.
  The step runs on the rows the last round returned until a round returns
  none; with `UNION`, rows returned before are dropped, so a walk of a cycle
  ends, and a `LIMIT` above ends one that would not.
- Aggregates: `COUNT(*)`, `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, each also with
  `DISTINCT`; their argument may have subqueries: `SUM((SELECT ...))`
- Window functions, in the select list and `ORDER BY`:
  `function(...) OVER ([window] [PARTITION BY expr, ...] [ORDER BY ...] [frame])`,
  or `OVER window`, a window the query names after `HAVING` with `WINDOW
  name AS (...), ...` (one may add an `ORDER BY` or a frame to another that
  has none). The functions are `ROW_NUMBER`, `RANK`, `DENSE_RANK`,
  `PERCENT_RANK`, `CUME_DIST`, `NTILE(n)`, `LAG(x[, offset[, default]])`,
  `LEAD(...)`, `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE(x, n)`, and the
  aggregates (without `DISTINCT`). A frame is `{ROWS | RANGE | GROUPS}
  [BETWEEN] start [AND end]`, each bound `UNBOUNDED PRECEDING`, `n
  PRECEDING`, `CURRENT ROW`, `n FOLLOWING` or `UNBOUNDED FOLLOWING`: `n` rows
  for `ROWS`, groups of peers for `GROUPS`, and for `RANGE`, over its one
  `ORDER BY` key, keys at most `n` away (a number, or an INTERVAL for dates
  and times; a NULL key's frame is the other NULLs). By default it runs from
  the start of the partition to the current row's last peer. In a grouped
  query they read the groups: `SUM(COUNT(*)) OVER ()`.
- Subqueries as a value, with `EXISTS` and with `[NOT] IN`, in the select
  list, `WHERE`, `ON`, `GROUP BY`, `HAVING`, `ORDER BY`, aggregates,
  `UPDATE`, `DELETE` and `VALUES`. They may be correlated and nested.

**Names**: a name is a word, as written (`users` and `Users` are two), or in
double quotes, which may hold any character and be a keyword: `"order"`,
`"Mixed Case"`, `"it""s"`. Comments are `-- to the end of the line` and
`/* ... */`, which may hold others.

**Expressions**: `+ - * /` (INT division truncates toward zero), `%` (of
INTs or DECIMALs, with the sign of the dividend), `||`,
`= != <> < <= > >=`, `AND OR NOT` with three-valued logic, `IS [NOT] NULL`,
`[NOT] IN (list)`, `[NOT] BETWEEN`, `[NOT] LIKE` (`%` and `_`; ASCII letters
match in either case, as in SQLite), `CASE [x] WHEN ... THEN ... [ELSE ...] END`.
`AND` and `OR` evaluate their right side only where the left side does not
decide, so `x <> 0 AND 10 / x > 1` never divides by zero. Integer overflow and
division by zero are errors. `x AT TIME ZONE zone` (see Types).

**Functions**: `COALESCE`, `NULLIF`, `LENGTH` (or `CHAR_LENGTH`), `UPPER`,
`LOWER`, `SUBSTR(text, start[, length])`, `ABS`, `ROUND(number[, digits])`.
They behave as in SQLite, without SQLite's type conversions; `LENGTH` of a
BLOB counts its bytes, and `ABS` and `ROUND` of a DECIMAL are exact. A NULL
argument makes NULL, but for `COALESCE`, `CONCAT`, `GREATEST` and `LEAST`.

| Kind | Functions |
|---|---|
| text | `TRIM(text[, characters])` or `TRIM([BOTH \| LEADING \| TRAILING] [characters] FROM text)`, `LTRIM`, `RTRIM`, `REPLACE(text, from, to)`, `INSTR(text, part)` or `POSITION(part IN text)` (from 1; 0 if nowhere), `LEFT(text, n)` and `RIGHT(text, n)` (a negative `n` leaves out `-n`), `REVERSE`, `REPEAT(text, n)`, `LPAD` and `RPAD(text, length[, fill])`, `CONCAT(value, ...)` (of any values, as TEXT, NULLs left out) |
| numbers | `CEIL` (or `CEILING`), `FLOOR`, `TRUNC(x[, digits])` (exact for DECIMALs), `SIGN`, `MOD(a, b)`; as REALs: `SQRT`, `POWER` (or `POW`), `EXP`, `LN`, `LOG(x)` (base 10) or `LOG(base, x)`, `LOG10`, `PI()` |
| any | `GREATEST(value, ...)` and `LEAST`, NULLs left out, as in PostgreSQL |
| time | `NOW()` or `CURRENT_TIMESTAMP` (a TIMESTAMPTZ), `CURRENT_DATE`, `CURRENT_TIME` (or `LOCALTIME`) and `LOCALTIMESTAMP`, in UTC: when the statement started, the same for all of it (in PostgreSQL, when the transaction did); `DATE_TRUNC(unit, x)`, from `microsecond` to `millennium` (`week` from Monday) |

- `CAST(x AS type)`: between numbers (a REAL or DECIMAL to an INT rounds,
  halves away from zero), any value to `TEXT` as a result shows it, a `TEXT`
  to any type it spells (`CAST('2024-01-05' AS DATE)`), a `TIMESTAMP` or a
  `TIMESTAMPTZ` to its `DATE` or `TIME` and to the other, a `DATE` to
  either, a `TIME` to an `INTERVAL` and back, a `TEXT` to its UTF-8 bytes as
  a `BLOB` and back, an `INT` to a `BOOL` and back
- `EXTRACT(field FROM x)`: `YEAR`, `QUARTER`, `MONTH`, `DAY`, `DOW` (0 for
  Sunday) or `DOY` of a date or timestamp, `HOUR`, `MINUTE` or `SECOND`
  (whole seconds) of a time or timestamp, the parts of an interval, as an
  INT; `EPOCH` (seconds since 1970, or an interval's) as a REAL

**Transactions**: every statement is atomic, and outside a transaction it is
durable once it returns. A transaction is durable once `COMMIT` returns; a
crash, `ROLLBACK` or closing the database undoes it. A statement that fails
inside a transaction is undone alone, and the transaction goes on.

Transactions run under snapshot isolation (PostgreSQL's `REPEATABLE READ`):
a transaction reads the database as it was when its first statement began,
with its own writes, and never sees what other transactions commit later.
Readers never wait for writers. Of two transactions that change the same
row, the first wins: the second waits for the first to end, and fails with
"could not serialize access" if it committed (outside a transaction, the
statement runs again on the new row instead). A transaction that fails so
can be rolled back and run again. Write skew is possible, as under any
snapshot isolation: two transactions may each read what the other changes.
See [Transactions and MVCC](#transactions-and-mvcc).

**Not supported**: `ALTER TABLE`, foreign keys, primary keys of several
columns, time zones by name (only UTC and offsets),
`SET TIME ZONE`, frame `EXCLUDE` clauses, and text stored in a DATE, TIME,
TIMESTAMP, TIMESTAMPTZ or INTERVAL column without a cast or a typed literal.

## Source layout

Each layer uses only the layers below it:

```
sql -> executor -> plan -> catalog -> mvcc -> storage        (types, predicate, error: shared by all)
```

```
src/
  main.rs            shell, server and client command line
  editor.rs          the shell's line editing and history (arrow keys)
  lib.rs             module list and public exports
  database.rs        Database: one session on one core; atomic statements, transactions, savepoints
  shared.rs          SharedDatabase and Session: many threads; statements take turns, row waits
  server.rs          TCP server (a thread per connection) and client
  types.rs           values, data types, columns; how values compare, convert and print
  decimal.rs         DECIMAL: exact decimal numbers of up to 38 digits, and their arithmetic
  datetime.rs        DATE, TIME, TIMESTAMP, TIMESTAMPTZ and INTERVAL: the calendar, time
                     zone offsets, interval arithmetic, parsing and printing
  predicate.rs       `column op literal` conditions, shared by the planner and storage
  result.rs          query results and table printing
  error.rs           the error type

  sql/
    lexer.rs         text -> tokens
    parser.rs        tokens -> AST (recursive descent)
    ast.rs           statements and expressions; printing them back as SQL
    eval.rs          evaluating an expression on a row, three-valued logic, scalar functions

  plan/
    filter.rs        what a statement needs from a table: columns read, conditions implied by WHERE
    access.rs        full scan, index, or LSM key lookup / key range; EXPLAIN of a scan
    query.rs         SELECT -> tree of plan steps: binding, pushdown, joins, costs, gathers, EXPLAIN
    selectivity.rs   the share of rows a condition or a join keeps, from statistics or guessed
    order.rs         the cheapest order to join tables in: dynamic programming over sets, or greedy

  catalog/
    mod.rs           tables, indexes and statistics, stored as their CREATE statements (and text) in a heap
    table.rs         a table: schema, storage engine, indexes, constraint checks, row counts
    index.rs         index entries (keys, included columns, row id) in a B+ tree or hash file
    stats.rs         ANALYZE: sampling, distinct values, common values, histograms; estimates from them

  mvcc/
    mod.rs           transactions: ids running and aborted, waits and deadlocks, vacuum horizon
    snapshot.rs      which versions a statement sees; whether it may change them
    clog.rs          the commit log: a bit per transaction, in pages of the file

  executor/
    mod.rs           runs statements; INSERT, UPDATE, DELETE, RETURNING, EXPLAIN ANALYZE
    model.rs         the three execution models and how a query picks one
    operator.rs      iterator model; the steps every model shares: joins (RIGHT and FULL
                     too), sorts, INTERSECT and EXCEPT, recursive queries, LATERAL
    materialize.rs   materialization model
    vectorized/      vectorized model: batches, column vectors, expressions over columns
    gather.rs        steps run on worker threads, their rows gathered
    aggregate.rs     hash aggregation, partial and final phases, column-at-a-time folds
    window.rs        window functions: partitions sorted, peers, frames
    sort.rs          external merge sort, top-N
    spill.rs         temporary files of rows; hash partitioning
    subquery.rs      runs subqueries, remembering answers per distinct outer values
    joins/           hash, index, merge, block and naive joins; the parallel ones

  storage/
    io/              medium.rs (files, memory, simulated crashes), wal.rs, disk.rs (pages, free list)
    buffer/          pool.rs (sharded buffer pool), replacer.rs (ARC, LRU-K, CLOCK)
    page.rs          slotted page
    tuple.rs         row <-> bytes
    version.rs       the version header of every stored row (xmin, xmax)
    sort_key.rs      values -> bytes that sort like the values
    heap.rs          table heap: linked pages, free-space map
    btree.rs         B+ tree with latch crabbing
    hash/            hash files: extendible and linear hashing, buckets, directory
    lsm/             LSM tree, SSTables, Bloom filter, skip list, MemTable
    column/          column store (row groups, zone maps, delta store) and its encodings

  trie/              trie, radix tree and ART, used as LSM MemTables

tests/               end-to-end SQL, differential tests against SQLite, concurrency, server
benches/             benchmarks (see below)
```

## How it works

### Files and durability

A database is one file of 4 KB pages, plus a write-ahead log beside it
(`<file>-wal`). Page 0 is a header (magic, version, page size, page count,
free list, root page). The last 4 bytes of every page are reserved for a
checksum that is not computed yet.

The database file is never written directly. Every page write is appended to
the log as a frame holding the whole page. A commit appends a commit frame
and syncs the log. Each frame's checksum covers the frame and the previous
frame's checksum, so recovery reads frames until one fails, keeps what was
committed and cuts off the rest. A savepoint is a position in the log;
rolling back to it truncates the log there. A checkpoint copies the newest
committed version of each page into the database file. It runs when the log
reaches 4 MB after a commit, on `CHECKPOINT`, when the database is closed,
and when it is opened after a crash. The file is locked, so only one process
can open it.

Every statement that writes ends with a commit frame, so the next statement
of any session starts from its pages; only a commit of a transaction syncs
(see [Transactions and MVCC](#transactions-and-mvcc)). When a statement
fails, the buffer pool drops its cached pages, the log drops the frames
since the last commit frame, and the catalog is read again from the
committed pages. The tests in `database.rs` crash a simulated disk at many
points inside statements and transactions, and check that each one is
applied whole or not at all.

### Buffer pool

Pages are read and written through a buffer pool of 256 frames (1 MB) in 8
shards. A page goes to the shard picked by a hash of its id, and each shard
has its own lock, page table and replacer, so threads reading different
pages rarely wait for each other. Pinned pages are never evicted. Dirty pages
are written to the log when they are evicted or when a statement ends. The
replacement policy is ARC by default; LRU-K and CLOCK can be chosen with
`--replacer` or `.replacer`, and the pool reports its hit rate.

### Storage engines

Every stored row is a version, with a 16-byte header of the transactions
that wrote it (xmin) and deleted it (xmax); see [Transactions and
MVCC](#transactions-and-mvcc).

- **Heap** (default): a linked list of slotted pages. A row is found by its
  page and slot. An in-memory free-space map sends each insert to the first
  page with room. An update writes a new version where there is room, and
  sets the old one's xmax; it stays in place, when its page has room, if no
  other transaction could see the old version.
- **LSM tree** (`USING LSM`): keyed by the primary key, or by a hidden row id,
  then by the version's xmin, so that a key's versions sort together, newest
  first. Writes go to a MemTable (ART by default) and to a log. The log is a heap
  inside the database file, so it is covered by the write-ahead log too. At
  64 KB the MemTable is written out as a sorted SSTable. Level compaction
  (default) or universal compaction merges SSTables. Lookups check the
  MemTable, then SSTables from newest to oldest, skipping those whose key
  range or Bloom filter excludes the key. Scans merge all sources in key
  order and start at the requested key.
- **Column store** (`USING COLUMN`): new rows go to a delta store (a heap).
  Every 1024 rows are moved into a row group, where each column is a chunk
  compressed with whichever encoding makes it smallest. A row group keeps a
  zone map (each column's least and greatest values and NULL count). Scans
  skip row groups by their zone maps, evaluate `column op literal`
  conditions on the compressed chunks (once per run, on the literal's
  bitmap, or on dictionary codes), and then decode the other columns only
  for the rows that match. Deletes set a bit in the group's delete bitmap;
  updates delete and insert again. Only rows every transaction sees are
  moved into row groups, which have no version headers; a delete of a row
  group's row stays pending, its xmax in a small heap of its own, until
  every transaction sees it and vacuum sets its bit. The delta store is
  moved after a commit, once 1024 of its rows are settled.

### Indexes

A **B+ tree** index stores one byte string per row: the keys encoded so
that bytes sort like values (NULL first), then the `INCLUDE` columns, then
the row id. Lookups are range scans over the leaves. Nodes split when full
and merge or borrow when under a quarter full. The root page never moves.
Threads use the tree at the same time through latch crabbing; scans go down
again from the root for each leaf instead of following leaf links, because a
merge can free a leaf.

A **hash index** (`USING HASH`) stores the same entries in a hash file. It
only answers equality on all of its keys. Two schemes are implemented:
extendible hashing (default; a directory of 2^d slots that doubles and halves)
and linear hashing (buckets split one at a time as the load grows). A
bucket that cannot be split continues in overflow pages.

A heap table with a primary key gets a unique index `<table>_pkey`. Column
tables cannot have indexes, because moving rows into a row group changes
their ids. Every write updates every index of its table. Each row version
has entries of its own: an index lookup reads the version an entry points
to, and skips it if the snapshot does not see it (so a covering index still
reads the table's row header), and vacuum removes the entries of the
versions it removes. A unique index counts the versions that are there now,
committed or the writer's own, whether the writer's snapshot sees them or not.

Keys are encoded in `storage/sort_key.rs`. INTs, REALs and DECIMALs share
one ordering by value, so `1`, `1.0` and `DECIMAL '1.00'` have the same key.
A DECIMAL that is not a whole number is encoded like the REAL nearest it,
followed by its exact digits, so that two DECIMALs which round to the same
REAL still sort apart. When an index was written by a version from before
DECIMAL, whose REAL keys ended differently, it is rebuilt once when the
database is opened.

### Planning

A `SELECT` becomes a tree of steps. Column references are bound to their
tables. `WHERE` and the `ON` of inner joins are split at `AND`; a condition
on one table is pushed down to its scan, and a condition on several tables
goes to the join that brings in the last of them. Conditions on the right
side of a `LEFT JOIN` are kept in the join. Nothing is pushed below a join
that may pad a table with NULLs: a condition of `WHERE` on the tables before
a `RIGHT` or `FULL JOIN` (or on none) is checked after it, and the `ON` of
an outer join stays in it, but for a `LEFT JOIN`'s conditions on its right
table alone.

Common table expressions are put in by the parser wherever they are named,
as derived tables, so the planner sees none; a recursive one becomes a
`RECURSIVE UNION` step over its anchor and its step, whose `WORK TABLE`
reads the rows of the last round. Window functions are computed above the
joins, the grouping and `HAVING`, by a `WINDOW` step for each distinct
`PARTITION BY` and `ORDER BY`, which the expressions above read as columns.

**Access paths.** Conditions of the form `expr op literal` choose how a table
is read: an LSM primary key lookup or key range, or the index that narrows
the rows most (equalities on its first keys, then a range on the next key).
A partial index is used only when the `WHERE` implies its condition. An index
that holds every column the query reads is used without reading the table
(`COVERING INDEX`). Otherwise the table is scanned. On an analyzed table,
an index that would find more than a quarter of the rows is left for a scan,
unless it is covering or partial.

**Statistics.** `ANALYZE` reads every row of a table and keeps a sample of
30000 (reservoir sampling, with a fixed seed). For each column it records the
share of NULLs, the number of distinct values (exact for a table sampled
whole, else estimated with the Haas-Stokes estimator, and kept as a share of
the rows when it is large), up to 16 most common values with their shares,
and an equi-depth histogram of up to 32 buckets of the rest. The statistics
are one catalog row per table, so they survive reopening, go with a
rollback and are dropped with the table; writes do not change them until the
next `ANALYZE`, but estimates use the current row count. With them, the share
of rows a condition keeps is estimated from its values: equalities and `IN`
from the common values or the distinct count, ranges (the bounds on a column
in an `AND` together, as for `BETWEEN`) from the histogram, `IS NULL` from
the null share, and `AND`, `OR` and `NOT` as if independent. A join on
equalities keeps `1 / max(distinct left, distinct right)` of the pairs. A
condition on tables never analyzed is guessed as before (below).
`EXPLAIN ANALYZE` shows each step's estimate next to the rows it returned.

**Join order.** The inner joins up to the first outer join or `LATERAL`
subquery may be done in any order. The planner finds the cheapest left-deep order by dynamic
programming over the sets of tables (greedily beyond 10 tables), never adding
a table no condition links to those joined so far while a linked one waits.
Each join is costed with the method it would use and the rows it would
make. The order of `FROM` is kept unless another is estimated to cost less
than three quarters of it, or when a join method is forced; `SELECT *` still
returns the columns in the order of `FROM`. For each join with an equality
between the two sides:

- an **index join** if the right table has an index (or LSM primary key) on
  the column and looking up each left row costs less than building a hash
  table of the right side;
- otherwise a **hash join**;
- a **merge join** only when its output order lets `ORDER BY` skip its sort.

A `RIGHT` or `FULL JOIN` uses a hash, merge or nested loop join, never an
index join: it must read all of its right side (see Execution).

Without an equality, a **block nested loop join** is used. The costs are
constants measured with `benches/joins.rs` (except for the cost of making a
joined row, and of trying a pair in a nested loop, which are rough guesses).
Table row counts are counted on first use. Without statistics, selectivity
is guessed (1/200 for an equality, 1/3 for anything else, one row for an
equality on a unique key), and a join on equalities returns as many rows as
its larger side.

### Execution

All three models run the same plan and return the same rows and errors.

| Model | A step returns | Chosen when |
|---|---|---|
| materialization | all its rows in one call | no step is expected to see more than 100 rows |
| vectorized | batches of 1024 rows, stored by column | some step sees 4096 rows or more, and the query has no join or subquery |
| iterator | one row per call | otherwise |

In the vectorized model, filters narrow a selection vector without copying
rows, `AND` and `OR` evaluate their right side only on undecided rows, and
arithmetic and comparisons on typed columns are plain loops the compiler can
vectorize. An aggregate without `GROUP BY` folds a whole column at once.
Joins and sorts use the iterator model's code in all three models.

Sorts, hash joins and hash aggregation have memory budgets. Past them, a
sort writes sorted runs to a temporary file and merges them; a hash join or
aggregation splits its rows into 16 partitions by hash and writes the
largest to temporary files, splitting again by more hash bits if needed.
Temporary files are deleted as soon as they are created.

Subqueries are planned and run like any query, after their outer references
are replaced by the current row's values. Answers are remembered per
distinct set of outer values, so an uncorrelated subquery runs once. A
`LATERAL` subquery is planned and run alike for each left row (`LATERAL
JOIN`), reusing its rows while the values it reads stay the same.

A `RIGHT JOIN` is an inner join, and a `FULL JOIN` a `LEFT JOIN`, of its two
sides; then come the right rows no left row matched, padded with NULLs. To
find them, each right row is joined the other way round with the left rows,
marked, by the same method with its keys swapped, and kept if no marked
row came with it. So both sides are read twice, within the method's budget.

A `WINDOW` step sorts its input by `PARTITION BY` and then `ORDER BY` (with
the sort's budget, writing runs past it), carrying the values of the
functions' arguments, and then holds one partition at a time. An aggregate
over a frame keeps its running state from one row to the next while the
frame only grows at its end, so a running total folds each row once; a
sliding frame folds its rows again for each row. A `RECURSIVE UNION` hands
on the rows of each round as they come, and runs the step again on them
once the round is over. `INTERSECT` and `EXCEPT` read their right side into
a hash table of rows and their counts, then stream the left side.

### Parallelism

With `.workers N` (or `set_scan_workers` and `set_join_workers`):

- A full scan of a heap, with the filters and projection above it, runs on
  N threads that each take the next page (`GATHER`). Rows come out in no
  order.
- A hash join of two such scans runs entirely on the workers: they build the
  hash table from the right side, then probe it with the left side.
- An index join of such a scan with a heap's index runs on the workers, each
  looking up the keys of its own rows.
- An aggregate over these is split: each worker computes a
  `PARTIAL AGGREGATE`, and a `FINALIZE AGGREGATE` merges the partial states.
- Any other hash join that does not need to keep order becomes a
  `PARALLEL HASH JOIN`, a grace hash join that partitions both sides and
  joins the partitions on N threads.

Steps with subqueries are not parallelized, because subqueries run in the
query's thread.

### Transactions and MVCC

Sequoia keeps versions of rows, as PostgreSQL does (`mvcc/`):

```text
row 7:  [xmin 12 | xmax 15 | name 'ann']   written by 12, deleted by 15
row 9:  [xmin 15 | xmax  0 | name 'amy']   the version 15 wrote instead

transaction 14, began before 15 committed:  sees row 7 ('ann')
transaction 16, began after:                sees row 9 ('amy')
```

- **Versions.** Every stored row starts with xmin, the transaction that
  wrote it, and xmax, the one that deleted it. An update sets the old
  version's xmax and writes a new version, with its own index entries.
- **Transaction ids.** A transaction gets an id when it first writes, and
  one more after each savepoint it writes after, so that `ROLLBACK TO`
  aborts only what followed.
- **Snapshots.** A transaction's first statement takes a snapshot: the next
  id, and the ids running then. It sees a version if it sees xmin and not
  xmax, and it sees an id that committed before the snapshot, or its own.
- **The commit log** keeps a bit for each transaction that committed, in
  pages of the database file. A commit sets its bits and syncs the log; a
  rollback writes nothing, and only marks its ids aborted in memory; a crash
  aborts every transaction that was running, since none has its bit. Every
  statement's pages are committed to the write-ahead log as it ends, without
  a sync, so the log no longer undoes transactions, only failed statements.
- **Conflicts.** A write may change only versions its snapshot sees, and no
  other transaction has deleted. One whose xmax belongs to a transaction
  still running fails, undone, and its session waits for that transaction
  and runs the statement again: it then fails for good if the other
  committed, and its transaction cannot see that commit. Inserting a key
  another running transaction wrote or deleted waits the same way. A
  transaction about to wait for one that waits for it, through others or
  not, fails at once with "deadlock detected".
- **Vacuum.** `VACUUM` removes the versions no transaction can see any
  more (written by an aborted transaction, or deleted before every running
  snapshot began) with their index entries, and moves a column table's
  settled rows into row groups. A table vacuums itself after a commit once
  updates and deletes have left 1000 old versions behind, and a fifth of its
  rows.
- **Alone.** A statement that no other transaction or snapshot could see a
  version of, and that the log undoes if it fails, writes in place: outside
  a transaction with no other open, or in a transaction that has the
  database to itself. Its rows need no transaction id and leave nothing for
  vacuum, so a single user writes as fast as before there were versions.
- **The schema** has no versions: CREATE, DROP and ANALYZE wait until no
  other session has a transaction open, and then keep the database to
  themselves until their transaction ends, its statements left uncommitted
  in the log for a rollback to cut off.

A file from before versions is upgraded when it is opened: its rows are
copied into new tables, as versions every transaction sees.

### Concurrency

A `SharedDatabase` lets many sessions' transactions run at once. Their
statements take turns, as SQLite's do: any number of statements that read
at once, or one that writes, which keeps the core until it ends (not until
its transaction does). A statement waiting to write goes before readers
that arrive after it. A session waits at most its busy timeout (5 seconds by
default), for its turn and for rows other transactions hold, and then fails
with "database is locked" or the conflict. A session dropped inside a
transaction rolls it back. Below this, the buffer pool, B+ trees and hash
files are safe for concurrent use with page latches, which parallel queries
rely on.

## Settings and switches

Per session, at runtime (on `Database` and `Session`):

| Setting | Default | Method |
|---|---|---|
| sort memory | 4 MB | `set_sort_memory` |
| block nested loop join memory | 4 MB | `set_join_memory` |
| hash join and hash aggregation memory | 16 MB | `set_hash_memory` |
| execution model | chosen per query | `set_execution_model`, `.model` |
| scan and hash join workers | 1 | `set_scan_workers`, `set_join_workers`, `.workers` |
| forced join method | none | `set_force_join` |
| busy timeout (sessions only) | 5 s | `set_busy_timeout` |

Per database, at runtime: the replacement policy (`set_replacement_policy`,
`--replacer`, `.replacer`).

At compile time, by editing a constant:

| Constant | Default | File |
|---|---|---|
| `REPLACEMENT_POLICY` | `Policy::Arc` | `src/storage/buffer/pool.rs` |
| `BUFFER_POOL_FRAMES`, `BUFFER_POOL_SHARDS` | 256, 8 | `src/database.rs` |
| `MEMTABLE` | `MemTableKind::Art` | `src/storage/lsm/memtable.rs` |
| `COMPACTION` | `Compaction::Level` | `src/storage/lsm/tree.rs` |
| `HASH_SCHEME` | `HashScheme::Extendible` | `src/storage/hash/mod.rs` |
| `CHECKPOINT_BYTES` | 4 MB | `src/storage/io/disk.rs` |
| `AUTOVACUUM_CHANGES` | 1000 | `src/catalog/table.rs` |

Files written with another MemTable, compaction strategy or hash scheme still
open.

## Limits

- **Row size**: a row must fit in one page, at most 4064 bytes encoded (4080
  with its version header; a few bytes less in an LSM table, which stores
  the key with it). There are no overflow pages.
- **Index entries** are at most 1024 bytes (keys, included columns and row id).
- **Column tables** hold at most about 1000 row groups (about a million rows),
  because the manifest is one page.
- **Write statements take turns**: transactions run side by side, but the
  statements that write run one at a time, and readers wait for the one
  running. A statement that changes the schema needs the database to itself.
- **Snapshot isolation, not serializable**: write skew is possible. A row
  conflict is not waited out inside the statement: the statement fails and
  runs again, from the start, once the other transaction ends.
- **Transaction ids** are not reused: the commit log holds about 33 million
  of them, and the ids of every aborted transaction are kept in memory.
- **Steps that hold rows in memory, with no budget**: `INTERSECT` and
  `EXCEPT` hold their right side's rows; a recursive `UNION` (not `ALL`)
  every row it returned; a window step one partition; a `LATERAL` join the
  rows its subquery returned for one left row.
- **Statistics only on request**: they are computed by `ANALYZE` and not
  kept up to date by writes (autovacuum removes versions only); estimates treat the
  conditions on different columns as independent (no multi-column
  statistics). Joins are reordered into left-deep trees only (no bushy
  plans), and not across a `LEFT JOIN`.
- **Small default cache**: the buffer pool is 1 MB unless the constants are changed.
- **No page checksums** in the database file; only the log is checksummed.
- **Startup work**: LSM SSTable summaries and Bloom filters are not stored and
  are rebuilt by reading every SSTable when a table is opened; heap
  free-space maps are rebuilt on first write.
- Heap pages are never released back to the free list while the table exists.

## Testing

```bash
cargo test --release
```

- Unit tests sit next to the code, including crash and I/O error tests on a
  simulated disk (`storage/io/medium.rs`) that keeps only what was synced.
- `tests/sql.rs` runs SQL end to end. Set `SEQUOIA_MODEL` to run every query
  in one execution model:

  ```bash
  SEQUOIA_MODEL=vectorized cargo test --release --test sql
  ```

- `tests/differential.rs` generates random schemas, rows, queries, changes
  and transactions, runs them on Sequoia and on SQLite (through the `sqlite3`
  command, skipped if missing), and fails if the results differ. Each case
  also runs in every execution model, and with memory budgets small enough
  to force spilling, which must return exactly the same rows and errors. It
  runs 40 cases by default:

  ```bash
  SEQUOIA_DIFF_CASES=2000 SEQUOIA_DIFF_SEED=7 cargo test --release --test differential -- --nocapture
  ```

  With `SEQUOIA_DIFF_TRACE=1` (and `--nocapture`) each statement is printed
  before it runs, to find one that never ends.

- `tests/concurrency.rs` and `tests/server.rs` test sessions side by side
  (snapshots, row conflicts, deadlocks and vacuum, in every storage engine)
  and the TCP server.

CI (`.github/workflows/ci.yml`) runs `cargo test --release` on Linux, macOS
and Windows, with `sqlite3` installed so the differential tests run.

## Benchmarks

Each benchmark is a plain program (`harness = false`). Pass group names to run
only those groups, and `--no-sqlite` to skip the SQLite comparison where there
is one:

```bash
cargo bench --bench joins
cargo bench --bench sort -- spill top
cargo bench --bench scan -- --no-sqlite
```

| Bench | Measures | SQLite column |
|---|---|---|
| `joins` | every join method on the same queries, 200,000 rows by 50,000, including parallel hash and index joins | |
| `sort` | `ORDER BY` in memory, with spilling at smaller and smaller budgets, and top-N | yes |
| `aggregate` | `GROUP BY` with few to many groups, each aggregate, `DISTINCT`, `HAVING` | yes |
| `scan` | reading rows from each storage engine, zone maps, filters, projections, `LIMIT`, parallel scans | yes |
| `lookup` | point and range lookups through B+ tree, hash and LSM keys, against full scans | yes |
| `writes` | inserts per engine and index count, small commits, updates, deletes, checkpoints | yes |
| `subquery` | uncorrelated, correlated and derived-table subqueries | yes |
| `models` | the same queries in each execution model | |
| `buffer_pool` | hit rate and cost of each replacement policy on access patterns | |
| `lsm` | level against universal compaction: write amplification, reads | |
| `column_encoding` | size and speed of each column encoding | |
| `hash` | extendible and linear hashing against a B+ tree | |
| `trie` | trie, radix tree and ART against `BTreeMap` and the skip list | |

Each prints the fastest of a few runs, and most check that the plan uses the
method they mean to time.
