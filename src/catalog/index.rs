//! Secondary indexes: a B+ tree, or a hash file (`USING HASH`), over the
//! keys of a table's rows.
//!
//! An index's keys are columns, or expressions of the row's columns
//! (`lower(name)`), one or several. Each entry is one key of the tree:
//!
//! ```text
//! [key 1][key 2]...[included columns][row id]
//! ```
//!
//! Keys are written as sort keys (see `storage/sort_key.rs`): their bytes
//! sort as the values do, NULL first, and each ends where it can be told to.
//! So the entries of rows with equal first keys are next to each other, in
//! the order of the next key, and are found by a scan of the entries that
//! start with those keys' encoding. The included columns (`INCLUDE`) follow
//! as a row of their own (see `storage/tuple.rs`); with the keys that are
//! plain columns, they let a query that reads nothing else be answered from
//! the index alone. The row id makes every entry unique even when many rows
//! share their keys. The index stores row ids as bytes; the table decides
//! what they mean.
//!
//! A hash index holds the same entries in a hash file (see
//! `storage/hash/mod.rs`), each hashed by its keys' encoding, which equal
//! values share (an INT and the REAL it equals included). It finds the
//! entries whose every key equals a value, in no order, and nothing else:
//! no first keys alone, and no ranges.
//!
//! A partial index has a condition (`WHERE`), and holds only the rows it is
//! true for. Every other row has an entry, NULL keys included.
//!
//! Keys and conditions are stored with their columns unqualified, as the
//! planner writes the conditions it compares them with.

use std::sync::Arc;

use crate::error::{Error, Result, err};
use crate::sql::ast::{ColumnRef, CreateIndex, Expr, IndexKind};
use crate::sql::eval::{Field, eval, locate, matches};
use crate::storage::hash::{self, HASH_SCHEME, HashFile};
use crate::storage::{BPlusTree, BufferPool, PageId, btree::MAX_KEY_SIZE, sort_key, tuple};
use crate::types::{Column, DataType, Value, column_index};

/// One end of a range of values.
#[derive(Debug, Clone, PartialEq)]
pub struct Bound {
    pub value: Value,
    pub inclusive: bool,
}

/// What an index is sorted by, one key after another.
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    /// A column of the table, by position.
    Column(usize),
    /// An expression of the table's columns.
    Expr(Expr),
}

pub struct Index {
    pub name: String,
    /// Whether two indexed rows may not have equal keys, unless one is NULL.
    pub unique: bool,
    pub keys: Vec<Key>,
    /// The keys as expressions, their columns unqualified (see [`Index::key_exprs`]).
    exprs: Vec<Expr>,
    /// The columns each entry holds besides the keys, by position.
    pub include: Vec<usize>,
    /// Only rows this is true for are indexed.
    pub predicate: Option<Expr>,
    /// The table's columns, unqualified, to evaluate keys and the predicate on a row.
    fields: Vec<Field>,
    /// `keys` and `predicate` with their columns found among `fields`
    /// already (see [`locate`]), as each row's entry is made from them.
    located: (Vec<Key>, Option<Expr>),
    types: Vec<DataType>,
    /// The included columns, to encode and decode them.
    included: Vec<Column>,
    /// Every column the keys, included columns and predicate read.
    reads: Vec<usize>,
    store: Store,
}

/// Where an index's entries are kept.
enum Store {
    BTree(BPlusTree),
    Hash(HashFile),
}

impl Index {
    /// Creates an empty index on a table with these columns; the table adds
    /// its rows. A hash index's file is of the scheme [`HASH_SCHEME`] says.
    pub fn create(
        pool: Arc<BufferPool>,
        definition: &CreateIndex,
        columns: &[Column],
    ) -> Result<Self> {
        Self::new(definition, columns, || match definition.using {
            IndexKind::BTree => Ok(Store::BTree(BPlusTree::create(pool)?)),
            IndexKind::Hash => Ok(Store::Hash(HashFile::create(pool, HASH_SCHEME)?)),
        })
    }

    /// Opens an index the catalog recorded, whose root page is `root`.
    pub fn open(
        pool: Arc<BufferPool>,
        definition: &CreateIndex,
        columns: &[Column],
        root: PageId,
    ) -> Result<Self> {
        Self::new(definition, columns, || match definition.using {
            IndexKind::BTree => Ok(Store::BTree(BPlusTree::open(pool, root))),
            IndexKind::Hash => Ok(Store::Hash(HashFile::open(pool, root)?)),
        })
    }

    /// Checks a definition against the table's columns, and only then makes
    /// the index's tree or file with `store`.
    fn new(
        definition: &CreateIndex,
        columns: &[Column],
        store: impl FnOnce() -> Result<Store>,
    ) -> Result<Self> {
        let CreateIndex {
            name,
            table,
            using: _,
            keys,
            include,
            predicate,
            unique,
        } = definition;
        let own = |expr: &Expr, what: &str| own_expr(expr, table, columns, what);
        let keys: Vec<Key> = keys
            .iter()
            .map(|key| {
                Ok(match own(key, "an index key")? {
                    Expr::Column(column) => Key::Column(column_index(columns, &column.name)?),
                    expr => Key::Expr(expr),
                })
            })
            .collect::<Result<_>>()?;
        let include: Vec<usize> = include
            .iter()
            .map(|c| column_index(columns, c))
            .collect::<Result<_>>()?;
        let predicate = predicate
            .as_ref()
            .map(|p| own(p, "an index's WHERE"))
            .transpose()?;

        let mut reads = Vec::new();
        for key in &keys {
            match key {
                Key::Column(i) => reads.push(*i),
                Key::Expr(expr) => columns_of(expr, columns, &mut reads),
            }
        }
        if let Some(predicate) = &predicate {
            columns_of(predicate, columns, &mut reads);
        }
        reads.extend(&include);
        reads.sort();
        reads.dedup();

        let exprs = keys
            .iter()
            .map(|key| match key {
                Key::Column(i) => Expr::Column(ColumnRef::bare(&columns[*i].name)),
                Key::Expr(expr) => expr.clone(),
            })
            .collect();
        let fields: Vec<Field> = columns.iter().map(|c| Field::named(&c.name)).collect();
        let located_keys = keys
            .iter()
            .map(|key| match key {
                Key::Expr(expr) => Key::Expr(locate(expr, &fields)),
                key => key.clone(),
            })
            .collect();
        let located = (located_keys, predicate.as_ref().map(|p| locate(p, &fields)));
        Ok(Index {
            name: name.clone(),
            unique: *unique,
            included: include.iter().map(|&i| columns[i].clone()).collect(),
            keys,
            exprs,
            include,
            predicate,
            fields,
            located,
            types: columns.iter().map(|c| c.data_type).collect(),
            reads,
            store: store()?,
        })
    }

    pub fn root_page(&self) -> PageId {
        match &self.store {
            Store::BTree(tree) => tree.root_page(),
            Store::Hash(file) => file.root_page(),
        }
    }

    pub fn kind(&self) -> IndexKind {
        match &self.store {
            Store::BTree(_) => IndexKind::BTree,
            Store::Hash(_) => IndexKind::Hash,
        }
    }

    /// Whether it is a hash index, which finds only rows whose every key equals a value.
    pub fn is_hash(&self) -> bool {
        self.kind() == IndexKind::Hash
    }

    /// The columns that the index's entries are made from: a row's other
    /// columns are not needed to find its entry.
    pub fn reads(&self) -> &[usize] {
        &self.reads
    }

    /// Whether the index is on this column alone, and holds every row.
    pub fn is_on_column(&self, column: usize) -> bool {
        self.keys == [Key::Column(column)] && self.predicate.is_none()
    }

    /// Whether its entries hold every column marked in `needed`, so a scan
    /// can return rows without reading the table.
    pub fn covers(&self, needed: &[bool]) -> bool {
        needed.iter().enumerate().all(|(i, &needed)| {
            !needed || self.include.contains(&i) || self.keys.contains(&Key::Column(i))
        })
    }

    /// The keys as expressions, their columns unqualified, as the planner
    /// writes the conditions it compares keys with.
    pub fn key_exprs(&self) -> &[Expr] {
        &self.exprs
    }

    /// The row's keys, or `None` if the row is not indexed: the predicate
    /// is not true for it.
    pub fn key_values(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
        let (keys, predicate) = &self.located;
        if !matches(predicate, &self.fields, row)? {
            return Ok(None);
        }
        let keys = keys.iter().map(|key| match key {
            Key::Column(i) => Ok(row[*i].clone()),
            Key::Expr(expr) => eval(expr, &self.fields, row),
        });
        Ok(Some(keys.collect::<Result<_>>()?))
    }

    /// The entry of a row with id `id`, or `None` if the row is not indexed.
    pub fn entry(&self, row: &[Value], id: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(keys) = self.key_values(row)? else {
            return Ok(None);
        };
        let mut entry = encode_keys(&keys);
        let included: Vec<Value> = self.include.iter().map(|&i| row[i].clone()).collect();
        entry.extend(tuple::encode(&self.included, &included)?);
        entry.extend_from_slice(id);
        Ok(Some(entry))
    }

    /// Fails if a row with an id `id_len` bytes long would make too long an entry.
    pub fn check(&self, row: &[Value], id_len: usize) -> Result<()> {
        if let Some(entry) = self.entry(row, &vec![0; id_len])?
            && entry.len() > MAX_KEY_SIZE
        {
            let len = entry.len();
            return err(format!(
                "value is too long for index {}: {len} bytes, the maximum is {MAX_KEY_SIZE}",
                self.name
            ));
        }
        Ok(())
    }

    /// Adds a row's entry, if it is indexed.
    pub fn insert(&mut self, row: &[Value], id: &[u8]) -> Result<()> {
        if let Some(entry) = self.entry(row, id)? {
            self.insert_entry(&entry)?;
        }
        Ok(())
    }

    /// Removes a row's entry, if it is indexed.
    pub fn delete(&mut self, row: &[Value], id: &[u8]) -> Result<()> {
        if let Some(entry) = self.entry(row, id)? {
            self.delete_entry(&entry)?;
        }
        Ok(())
    }

    /// Adds an entry made by [`Index::entry`].
    pub fn insert_entry(&mut self, entry: &[u8]) -> Result<()> {
        match &self.store {
            Store::BTree(tree) => tree.insert(entry).map(|_| ()),
            Store::Hash(file) => file.insert(hash::hash(&entry[..self.keys_end(entry)?]), entry),
        }
    }

    /// Removes an entry made by [`Index::entry`].
    pub fn delete_entry(&mut self, entry: &[u8]) -> Result<()> {
        match &self.store {
            Store::BTree(tree) => tree.delete(entry).map(|_| ()),
            Store::Hash(file) => file
                .delete(hash::hash(&entry[..self.keys_end(entry)?]), entry)
                .map(|_| ()),
        }
    }

    /// The ids of the rows whose first keys are these, encoded by
    /// [`encode_keys`]; for a hash index, every key.
    pub fn find(&self, keys: &[u8]) -> Result<Vec<Vec<u8>>> {
        let entries = match &self.store {
            Store::BTree(tree) => {
                let mut entries = Vec::new();
                for entry in tree.range(keys) {
                    let entry = entry?;
                    if !entry.starts_with(keys) {
                        break;
                    }
                    entries.push(entry);
                }
                entries
            }
            Store::Hash(file) => file.find(hash::hash(keys), keys)?,
        };
        entries
            .iter()
            .map(|entry| Ok(self.row_id(entry)?.to_vec()))
            .collect()
    }

    /// The entries whose first keys equal `equal`, and whose next key lies
    /// between the bounds, in index order. A missing bound leaves that end
    /// open; a range with a bound leaves out NULL, which no comparison matches.
    ///
    /// A hash index takes only values for every key and no bounds, and
    /// returns its entries in no order.
    pub fn scan(
        &self,
        equal: &[Value],
        lower: Option<&Bound>,
        upper: Option<&Bound>,
    ) -> Result<Vec<Vec<u8>>> {
        let prefix = encode_keys(equal);
        let tree = match &self.store {
            Store::BTree(tree) => tree,
            Store::Hash(file) => {
                if equal.len() < self.keys.len() || lower.is_some() || upper.is_some() {
                    return err(format!(
                        "hash index {} finds only rows whose every key equals a value",
                        self.name
                    ));
                }
                return file.find(hash::hash(&prefix), &prefix);
            }
        };
        let encode = |b: &Bound| (encode_keys(std::slice::from_ref(&b.value)), b.inclusive);
        let (lower, upper) = (lower.map(encode), upper.map(encode));
        let mut start = prefix.clone();
        match &lower {
            Some((bound, _)) => start.extend_from_slice(bound),
            None if upper.is_some() => start.push(sort_key::ABOVE_NULL),
            None => {}
        }

        let mut entries = Vec::new();
        for entry in tree.range(&start) {
            let entry = entry?;
            if !entry.starts_with(&prefix) {
                break;
            }
            if lower.is_some() || upper.is_some() {
                // The next key's encoding, which compares as its value does.
                let key = &entry[prefix.len()..sort_key::skip(&entry, prefix.len())?];
                if let Some((bound, false)) = &lower
                    && key == bound.as_slice()
                {
                    continue;
                }
                if let Some((bound, inclusive)) = &upper
                    && (key > bound.as_slice() || (!inclusive && key == bound.as_slice()))
                {
                    break;
                }
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    /// The row id at the end of an entry.
    pub fn row_id<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8]> {
        let keys = self.keys_end(entry)?;
        let (_, included) = tuple::decode_prefix(&self.included, &entry[keys..])?;
        Ok(&entry[keys + included..])
    }

    /// A row of the table read from an entry: the columns marked in
    /// `needed`, which the index must cover (see [`Index::covers`]), and NULL
    /// in the others.
    pub fn row(&self, entry: &[u8], needed: &[bool]) -> Result<Vec<Value>> {
        let mut row = vec![Value::Null; self.types.len()];
        let mut pos = 0;
        for key in &self.keys {
            let value = sort_key::decode(entry, &mut pos)?;
            if let Key::Column(i) = key
                && needed[*i]
            {
                // A REAL equal to an INT was stored as one.
                row[*i] = value
                    .coerce(self.types[*i])
                    .ok_or_else(|| Error("corrupted index entry".into()))?;
            }
        }
        let (included, _) = tuple::decode_prefix(&self.included, &entry[pos..])?;
        for (&i, value) in self.include.iter().zip(included) {
            row[i] = value;
        }
        Ok(row)
    }

    /// Where an entry's keys end.
    fn keys_end(&self, entry: &[u8]) -> Result<usize> {
        (0..self.keys.len()).try_fold(0, |pos, _| sort_key::skip(entry, pos))
    }

    /// Frees all of the index's pages.
    pub fn destroy(self) -> Result<()> {
        match self.store {
            Store::BTree(tree) => tree.destroy(),
            Store::Hash(file) => file.destroy(),
        }
    }
}

/// Encodes keys as an entry starts with them.
pub fn encode_keys(keys: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for key in keys {
        sort_key::encode(key, false, &mut bytes);
    }
    bytes
}

/// Adds the positions of the columns an expression of an index reads.
fn columns_of(expr: &Expr, columns: &[Column], out: &mut Vec<usize>) {
    expr.visit(&mut |e| {
        if let Expr::Column(column) = e {
            out.extend(column_index(columns, &column.name));
        }
    });
}

/// An expression of an index on `table`, with its columns unqualified;
/// fails if it reads anything but the table's columns, or aggregates.
fn own_expr(expr: &Expr, table: &str, columns: &[Column], what: &str) -> Result<Expr> {
    let mut problem = None;
    expr.visit(&mut |e| match e {
        Expr::Column(column) => {
            if column.table.as_ref().is_some_and(|t| t != table)
                || column_index(columns, &column.name).is_err()
            {
                problem.get_or_insert_with(|| format!("unknown column: {column}"));
            }
        }
        Expr::Aggregate(..) => {
            _ = problem.get_or_insert_with(|| format!("{what} cannot use an aggregate"))
        }
        Expr::Subquery(_) | Expr::Exists(_) | Expr::InSubquery(..) | Expr::Outer(_) => {
            _ = problem.get_or_insert_with(|| format!("{what} cannot use a subquery"))
        }
        _ => {}
    });
    if let Some(problem) = problem {
        return err(problem);
    }
    expr.replace(&mut |e| match e {
        Expr::Column(column) => Some(Ok(Expr::Column(ColumnRef::bare(&column.name)))),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Statement;
    use crate::sql::parse;
    use crate::storage::DiskManager;

    fn columns() -> Vec<Column> {
        [
            ("id", DataType::Int),
            ("name", DataType::Text),
            ("score", DataType::Real),
            ("n", DataType::Int),
        ]
        .map(|(name, data_type)| Column {
            name: name.into(),
            data_type,
            primary_key: false,
        })
        .into()
    }

    fn index(sql: &str) -> Index {
        let Some(Statement::CreateIndex(definition)) = parse(sql).unwrap().pop() else {
            panic!("{sql}")
        };
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 16));
        Index::create(pool, &definition, &columns()).unwrap()
    }

    fn row(id: i64, name: Option<&str>, score: f64, n: Option<i64>) -> Vec<Value> {
        let name = name.map_or(Value::Null, |s| Value::Text(s.into()));
        vec![
            Value::Int(id),
            name,
            Value::Real(score),
            n.map_or(Value::Null, Value::Int),
        ]
    }

    /// The first byte of each row id an index scan returns.
    fn ids(index: &Index, equal: &[Value], lower: Option<Bound>, upper: Option<Bound>) -> Vec<u8> {
        let entries = index.scan(equal, lower.as_ref(), upper.as_ref()).unwrap();
        entries
            .iter()
            .map(|e| index.row_id(e).unwrap()[0])
            .collect()
    }

    #[test]
    fn scans_fix_the_first_keys_and_bound_the_next() {
        let mut index = index("CREATE INDEX i ON t (name, n)");
        let rows = [
            row(0, Some("b"), 0.0, Some(2)),
            row(1, Some("a"), 0.0, Some(5)),
            row(2, Some("b"), 0.0, None),
            row(3, Some("b"), 0.0, Some(-1)),
            row(4, None, 0.0, Some(2)),
            row(5, Some("b"), 0.0, Some(7)),
        ];
        for row in &rows {
            index
                .insert(row, &[row[0].to_sql().parse::<u8>().unwrap(), 9, 9])
                .unwrap();
        }
        let b = [Value::Text("b".into())];
        let bound = |n, inclusive| {
            Some(Bound {
                value: Value::Int(n),
                inclusive,
            })
        };
        assert_eq!(
            ids(&index, &b, None, None),
            [2, 3, 0, 5],
            "NULL first, then by n"
        );
        assert_eq!(ids(&index, &b, bound(-1, false), bound(7, false)), [0]);
        assert_eq!(ids(&index, &b, bound(-1, true), bound(7, true)), [3, 0, 5]);
        assert_eq!(
            ids(&index, &b, None, bound(2, true)),
            [3, 0],
            "a range leaves NULL out"
        );
        assert_eq!(ids(&index, &[], None, None), [4, 1, 2, 3, 0, 5]);
        assert_eq!(ids(&index, &[Value::Null], None, None), [4]);

        index.delete(&rows[3], &[3, 9, 9]).unwrap();
        assert_eq!(ids(&index, &b, None, None), [2, 0, 5]);
        assert_eq!(
            index
                .find(&encode_keys(&[b[0].clone(), Value::Int(7)]))
                .unwrap(),
            [vec![5, 9, 9]]
        );
    }

    #[test]
    fn entries_hold_expressions_included_columns_and_ids_of_any_length() {
        let index =
            index("CREATE INDEX i ON t (LOWER(name), score) INCLUDE (n, name) WHERE n IS NOT NULL");
        assert_eq!(index.reads(), [1, 2, 3]);
        assert!(
            index
                .entry(&row(1, Some("A"), 1.0, None), b"id")
                .unwrap()
                .is_none(),
            "left out by the WHERE"
        );

        let entry = index
            .entry(
                &row(1, Some("A\0b"), 2.0, Some(-3)),
                b"an LSM key \0 of any length",
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            index.row_id(&entry).unwrap(),
            b"an LSM key \0 of any length"
        );
        let needed = [false, true, true, true];
        assert!(index.covers(&needed));
        assert!(!index.covers(&[true, false, false, false]));
        // The REAL key was stored as the INT it equals, and comes back a REAL.
        let decoded = index.row(&entry, &needed).unwrap();
        let mut expected = row(1, Some("A\0b"), 2.0, Some(-3));
        expected[0] = Value::Null; // not needed
        assert_eq!(decoded, expected);
        assert!(matches!(decoded[2], Value::Real(_)));
        let keys = index.key_values(&row(1, Some("XY"), 0.5, Some(1))).unwrap();
        assert_eq!(keys, Some(vec![Value::Text("xy".into()), Value::Real(0.5)]));
    }

    #[test]
    fn definitions_are_checked_against_the_columns() {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 16));
        let pages = pool.disk().page_count();
        for sql in [
            "CREATE INDEX i ON t (nope)",
            "CREATE INDEX i ON t (u.name)",
            "CREATE INDEX i ON t (n) WHERE SUM(n) > 1",
        ] {
            let Some(Statement::CreateIndex(definition)) = parse(sql).unwrap().pop() else {
                unreachable!()
            };
            assert!(
                Index::create(pool.clone(), &definition, &columns()).is_err(),
                "{sql}"
            );
        }
        assert_eq!(pool.disk().page_count(), pages, "no tree was made");
        let index = index("CREATE INDEX i ON t (t.n, n + 1) WHERE t.name > 'a'");
        assert_eq!(index.keys, [Key::Column(3), Key::Expr(parse_expr("n + 1"))]);
        assert_eq!(
            index.predicate,
            Some(parse_expr("name > 'a'")),
            "columns unqualified"
        );
    }

    fn parse_expr(sql: &str) -> Expr {
        let Some(Statement::Select(query)) = parse(&format!("SELECT * FROM t WHERE {sql}"))
            .unwrap()
            .pop()
        else {
            unreachable!()
        };
        query.filter.unwrap()
    }
}
