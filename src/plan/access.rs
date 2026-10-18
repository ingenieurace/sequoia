//! Access paths: how a statement reads its table, given the conditions its
//! WHERE clause implies.

use crate::catalog::index::Bound;
use crate::catalog::table::{Access, Table};
use crate::predicate::{Compare, Predicate};
use crate::types::StorageKind;

/// Picks how to read the rows that may satisfy `predicates`: by primary key
/// in an LSM tree if one predicate fixes it, otherwise through the index the
/// predicates narrow down most (an equality beats a range with two ends,
/// which beats one with one end; the earliest index wins a tie), otherwise by
/// reading every row. A column table is always read in full, with the
/// predicates pushed down into the scan.
pub fn choose(table: &Table, predicates: &[Predicate]) -> Access {
    match table.storage_kind() {
        StorageKind::Column => return Access::Scan,
        StorageKind::Lsm => {
            if let Some(pk) = table.primary_key()
                && let Some(p) = predicates.iter().find(|p| p.column == pk && p.op == Compare::Eq)
            {
                return Access::Key(p.literal.clone());
            }
        }
        StorageKind::Heap => {}
    }

    let mut best: Option<(usize, Access)> = None;
    for (i, index) in table.indexes().iter().enumerate() {
        let (mut lower, mut upper, mut equal) = (None, None, false);
        for p in predicates.iter().filter(|p| p.column == index.column) {
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
        let score = equal as usize + lower.is_some() as usize + upper.is_some() as usize;
        if score > best.as_ref().map_or(0, |(s, _)| *s) {
            best = Some((score, Access::Index { index: i, lower, upper }));
        }
    }
    best.map_or(Access::Scan, |(_, access)| access)
}

/// Describes the access path [`choose`] picks, as `EXPLAIN` prints it.
pub fn explain(table: &Table, predicates: &[Predicate]) -> String {
    let name = &table.name;
    match choose(table, predicates) {
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
        Access::Index { index, lower, upper } => {
            let index = &table.indexes()[index];
            let column = &table.columns[index.column].name;
            let range = match (&lower, &upper) {
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
            };
            format!("SEARCH {name} USING INDEX {} ({range})", index.name)
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
