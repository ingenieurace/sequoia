//! Access paths: how a statement reads its table, given the conditions its
//! WHERE clause implies.

use crate::catalog::index::{Bound, Index};
use crate::catalog::table::{Access, Table};
use crate::plan::filter::{self, Condition, conjuncts};
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{ColumnRef, Expr};
use crate::types::StorageKind;

/// Picks how to read the rows of `table`, which goes by `qualifier`, that
/// `filter` may match, given the columns marked in `needed` are read: by
/// primary key in an LSM tree if a condition fixes it, otherwise by the
/// range of keys, or through the index, that the conditions narrow down
/// most, otherwise by reading every row. A column table is always read in
/// full, with the conditions on its columns pushed down into the scan.
///
/// An index narrows the rows down by 3 for each of its first keys that a
/// condition fixes, then by 1 for each end of a range its next key is in;
/// and a partial index by 3 more, as its condition leaves out rows as an
/// equality would (`WHERE status = 'open'`). An LSM tree's key range
/// wins a tie, then an index that holds every column needed, then the
/// earliest index. A partial index serves only a WHERE that implies its
/// condition, so that it holds every row the WHERE may match: when each of
/// its condition's conjuncts is one of the WHERE's, or is implied by one of
/// its conditions (`x > 5` implies `x > 0`, `x = 1` implies `x != 2`), or
/// is `e IS NOT NULL` for an `e` the WHERE compares with a value.
pub fn choose(table: &Table, qualifier: &str, filter: &Option<Expr>, conditions: &[Condition], needed: &[bool]) -> Access {
    let mut own = None; // the WHERE's conjuncts, once a partial index needs them
    let mut best: Option<((usize, bool), Access)> = None;
    match table.storage_kind() {
        StorageKind::Column => return Access::Scan,
        StorageKind::Lsm => {
            if let Some(pk) = table.primary_key() {
                let key = Expr::Column(ColumnRef::bare(&table.columns[pk].name));
                if let Some(c) = conditions.iter().find(|c| c.expr == key && c.op == Compare::Eq) {
                    return Access::Key(c.literal.clone());
                }
                let (equal, lower, upper) = range(conditions, &key);
                let score = equal as usize + lower.is_some() as usize + upper.is_some() as usize;
                if score > 0 {
                    best = Some(((score, true), Access::KeyRange { lower, upper }));
                }
            }
        }
        StorageKind::Heap => {}
    }

    for (i, index) in table.indexes().iter().enumerate() {
        if let Some(predicate) = &index.predicate
            && !implied(table, qualifier, predicate, own.get_or_insert_with(|| filter::own_conjuncts(table, qualifier, filter)), conditions)
        {
            continue;
        }
        let (mut score, mut equal, mut lower, mut upper) = (0, Vec::new(), None, None);
        for key in index.key_exprs() {
            match range(conditions, key) {
                (true, Some(l), Some(u)) if l == u => {
                    score += 3;
                    equal.push(l.value);
                }
                (fixed, l, u) => {
                    score += fixed as usize + l.is_some() as usize + u.is_some() as usize;
                    (lower, upper) = (l, u);
                    break;
                }
            }
        }
        score += 3 * index.predicate.is_some() as usize;
        let rank = (score, index.covers(needed));
        if score > 0 && best.as_ref().is_none_or(|(best, _)| rank > *best) {
            best = Some((rank, Access::Index { index: i, equal, lower, upper, covering: rank.1 }));
        }
    }
    best.map_or(Access::Scan, |(_, access)| access)
}

/// The tightest bounds that `conditions` put on an expression, and whether
/// one of them is an equality.
fn range(conditions: &[Condition], expr: &Expr) -> (bool, Option<Bound>, Option<Bound>) {
    let (mut lower, mut upper, mut equal) = (None, None, false);
    for c in conditions.iter().filter(|c| c.expr == *expr) {
        let bound = |inclusive| Some(Bound { value: c.literal.clone(), inclusive });
        match c.op {
            Compare::Eq => {
                equal = true;
                lower = tighter(lower, bound(true), true);
                upper = tighter(upper, bound(true), false);
            }
            Compare::Gt => lower = tighter(lower, bound(false), true),
            Compare::GtEq => lower = tighter(lower, bound(true), true),
            Compare::Lt => upper = tighter(upper, bound(false), false),
            Compare::LtEq => upper = tighter(upper, bound(true), false),
            Compare::NotEq => {}
        }
    }
    (equal, lower, upper)
}

/// Whether a partial index's condition holds for every row that the
/// conjuncts of a WHERE clause, and the conditions among them, hold for.
fn implied(table: &Table, qualifier: &str, predicate: &Expr, own: &[Expr], conditions: &[Condition]) -> bool {
    conjuncts(predicate).into_iter().all(|part| {
        if own.contains(part) {
            return true;
        }
        if let Expr::IsNull(e, true) = part {
            return conditions.iter().any(|c| c.expr == **e);
        }
        match filter::conditions(table, qualifier, &Some(part.clone())).as_slice() {
            [needed] => conditions.iter().any(|c| implies(c, needed)),
            _ => false,
        }
    })
}

/// Whether every value that condition `a` holds for is one `b` holds for.
fn implies(a: &Condition, b: &Condition) -> bool {
    use Compare::*;
    let Some(ord) = (a.expr == b.expr).then(|| a.literal.compare(&b.literal)).flatten() else { return false };
    match (a.op, b.op) {
        // `x = 3` implies whatever 3 satisfies.
        (Eq, op) => op.holds(ord),
        (NotEq, NotEq) => ord.is_eq(),
        (Gt | GtEq, Gt | GtEq) => ord.is_gt() || (ord.is_eq() && (b.op == GtEq || a.op == Gt)),
        (Lt | LtEq, Lt | LtEq) => ord.is_lt() || (ord.is_eq() && (b.op == LtEq || a.op == Lt)),
        // `x > 3` implies `x != 2`, and `x != 3`.
        (Gt, NotEq) | (GtEq, NotEq) => ord.is_gt() || (ord.is_eq() && a.op == Gt),
        (Lt, NotEq) | (LtEq, NotEq) => ord.is_lt() || (ord.is_eq() && a.op == Lt),
        _ => false,
    }
}

/// Describes an access path for `predicates`, as `EXPLAIN` prints it, naming
/// the table `name`.
pub fn describe(table: &Table, name: &str, predicates: &[Predicate], access: &Access) -> String {
    match access.clone() {
        Access::Scan if table.storage_kind() == StorageKind::Column && !predicates.is_empty() => {
            let filters: Vec<String> = predicates
                .iter()
                .map(|p| format!("{} {} {}", table.columns[p.column].name, p.op.symbol(), p.literal.to_sql()))
                .collect();
            format!("SCAN {name} FILTERING COMPRESSED CHUNKS ON {}", filters.join(" AND "))
        }
        Access::Scan => format!("SCAN {name}"),
        Access::Key(value) => {
            let pk = table.primary_key().expect("a key lookup needs a primary key");
            format!("SEARCH {name} USING PRIMARY KEY ({} = {})", table.columns[pk].name, value.to_sql())
        }
        Access::KeyRange { lower, upper } => {
            let pk = table.primary_key().expect("a key range needs a primary key");
            format!("SEARCH {name} USING PRIMARY KEY ({})", describe_range(&table.columns[pk].name, lower, upper))
        }
        Access::Index { index, equal, lower, upper, covering } => {
            let index = &table.indexes()[index];
            let kind = if covering { "COVERING INDEX" } else { "INDEX" };
            let key = |i: usize| key_name(index, i);
            let mut parts: Vec<String> =
                equal.iter().enumerate().map(|(i, value)| format!("{} = {}", key(i), value.to_sql())).collect();
            if lower.is_some() || upper.is_some() {
                parts.push(describe_range(&key(equal.len()), lower, upper));
            }
            match parts.is_empty() {
                true => format!("SCAN {name} USING {kind} {}", index.name),
                false => format!("SEARCH {name} USING {kind} {} ({})", index.name, parts.join(" AND ")),
            }
        }
    }
}

/// An index's key as EXPLAIN shows it: a column's name, or an expression.
fn key_name(index: &Index, i: usize) -> String {
    index.key_exprs()[i].to_string()
}

/// Describes the bounds of a range of `column`: `column = v` for a single
/// value, or its ends joined by AND.
fn describe_range(column: &str, lower: Option<Bound>, upper: Option<Bound>) -> String {
    match (&lower, &upper) {
        (Some(l), Some(u)) if l == u && l.inclusive => format!("{column} = {}", l.value.to_sql()),
        _ => {
            let describe = |b: Bound, inclusive: Compare, exclusive: Compare| {
                let op = if b.inclusive { inclusive } else { exclusive };
                format!("{column} {} {}", op.symbol(), b.value.to_sql())
            };
            let lower = lower.map(|b| describe(b, Compare::GtEq, Compare::Gt));
            let upper = upper.map(|b| describe(b, Compare::LtEq, Compare::Lt));
            lower.into_iter().chain(upper).collect::<Vec<_>>().join(" AND ")
        }
    }
}

/// The tighter of two lower bounds (`lower`) or upper bounds: the larger
/// lower or smaller upper value, and of equal values the exclusive one.
fn tighter(current: Option<Bound>, new: Option<Bound>, lower: bool) -> Option<Bound> {
    let (Some(current), Some(new)) = (&current, &new) else { return current.or(new) };
    let ord = new.value.sort_cmp(&current.value);
    let better = if lower { ord.is_gt() } else { ord.is_lt() } || (ord.is_eq() && !new.inclusive);
    Some(if better { new.clone() } else { current.clone() })
}
