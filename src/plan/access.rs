//! Access paths: how a statement reads its table, given the conditions its
//! WHERE clause implies.

use crate::catalog::index::Bound;
use crate::catalog::table::{Access, Table};
use crate::predicate::{Compare, Predicate};
use crate::types::StorageKind;

/// Picks how to read the rows that may satisfy `predicates`: by primary key
/// in an LSM tree if one predicate fixes it, otherwise by the range of keys,
/// or through the index, that the predicates narrow down most (an equality
/// beats a range with two ends, which beats one with one end; an LSM tree's
/// key range wins a tie, then the earliest index), otherwise by reading
/// every row. A column table is always read in full, with the predicates
/// pushed down into the scan.
pub fn choose(table: &Table, predicates: &[Predicate]) -> Access {
    let mut best: Option<(usize, Access)> = None;
    match table.storage_kind() {
        StorageKind::Column => return Access::Scan,
        StorageKind::Lsm => {
            if let Some(pk) = table.primary_key() {
                if let Some(p) = predicates.iter().find(|p| p.column == pk && p.op == Compare::Eq) {
                    return Access::Key(p.literal.clone());
                }
                let (score, lower, upper) = range(predicates, pk);
                if score > 0 {
                    best = Some((score, Access::KeyRange { lower, upper }));
                }
            }
        }
        StorageKind::Heap => {}
    }

    for (i, index) in table.indexes().iter().enumerate() {
        let (score, lower, upper) = range(predicates, index.column);
        if score > best.as_ref().map_or(0, |(s, _)| *s) {
            best = Some((score, Access::Index { index: i, lower, upper }));
        }
    }
    best.map_or(Access::Scan, |(_, access)| access)
}

/// The tightest bounds that `predicates` put on a column, and how far they
/// narrow it: 1 for each end, and 1 more for an equality.
fn range(predicates: &[Predicate], column: usize) -> (usize, Option<Bound>, Option<Bound>) {
    let (mut lower, mut upper, mut equal) = (None, None, false);
    for p in predicates.iter().filter(|p| p.column == column) {
        let bound = |inclusive| Some(Bound { value: p.literal.clone(), inclusive });
        match p.op {
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
    (equal as usize + lower.is_some() as usize + upper.is_some() as usize, lower, upper)
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
        Access::Index { index, lower, upper } => {
            let index = &table.indexes()[index];
            let range = describe_range(&table.columns[index.column].name, lower, upper);
            format!("SEARCH {name} USING INDEX {} ({range})", index.name)
        }
    }
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
