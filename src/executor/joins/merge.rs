//! The sort-merge join.

use std::cmp::Ordering;

use crate::catalog::table::Row;
use crate::error::Result;
use crate::executor::operator::Rows;
use crate::executor::sort::{Sorted, Sorter};
use crate::sql::ast::{Expr, JoinKind, OrderBy};
use crate::sql::eval::{Field, NoSubqueries, Subqueries, eval, locate, locate_all, matches_with};
use crate::types::Value;

/// How a merge join sorts its sides: see [`JoinMethod::Merge`](crate::plan::query::JoinMethod::Merge).
pub struct MergeSort {
    pub left_keys: Vec<Expr>,
    pub right_keys: Vec<Expr>,
    pub desc: Vec<bool>,
    pub then: Vec<OrderBy>,
    /// Bytes each side's sort may buffer before it writes runs out.
    pub budget: usize,
}

/// A sort-merge join: sorts both sides by their keys, then walks them
/// together, pairing each left row with the right rows of equal keys.
pub struct MergeJoin<'a> {
    /// Each side, until it is sorted: the left when the first row is asked
    /// for, the right when the first left row with no NULL key is read.
    left_input: Option<Rows<'a>>,
    right_input: Option<Rows<'a>>,
    left: Option<Sorted>,
    right: Option<Sorted>,
    left_fields: Vec<Field>,
    right_fields: Vec<Field>,
    sort: MergeSort,
    right_width: usize,
    kind: JoinKind,
    condition: Option<Expr>,
    /// Runs the subqueries of the condition, if it has any.
    subqueries: Box<dyn Subqueries + 'a>,
    /// The left fields, then the right ones: the fields of a joined row.
    fields: Vec<Field>,
    /// The next right row, with its keys, once read and until it joins `group`.
    lookahead: Option<(Vec<Value>, Row)>,
    /// The right rows whose keys are `group_key`, the last left keys looked up.
    group_key: Option<Vec<Value>>,
    group: Vec<Row>,
    /// The left row being joined, how many rows of `group` match it, the next one to pair.
    current: Option<Row>,
    end: usize,
    next: usize,
    matched: bool,
}

impl<'a> MergeJoin<'a> {
    /// Runs the subqueries of the join's condition with `subqueries`.
    pub fn running(mut self, subqueries: Box<dyn Subqueries + 'a>) -> Self {
        self.subqueries = subqueries;
        self
    }

    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Rows<'a>,
        right_fields: Vec<Field>,
        sort: MergeSort,
        kind: JoinKind,
        condition: Option<Expr>,
    ) -> Self {
        let fields = [left_fields.clone(), right_fields.clone()].concat();
        let condition = condition.map(|c| locate(&c, &fields));
        // The left side sorts by its keys, then by `then`; the right one by its keys.
        let sort = MergeSort {
            left_keys: locate_all(&sort.left_keys, &left_fields),
            right_keys: locate_all(&sort.right_keys, &right_fields),
            then: sort
                .then
                .iter()
                .map(|o| OrderBy {
                    expr: locate(&o.expr, &left_fields),
                    desc: o.desc,
                })
                .collect(),
            ..sort
        };
        MergeJoin {
            left_input: Some(left),
            right_input: Some(right),
            left: None,
            right: None,
            fields,
            left_fields,
            right_width: right_fields.len(),
            right_fields,
            sort,
            kind,
            condition,
            lookahead: None,
            subqueries: Box::new(NoSubqueries),
            group_key: None,
            group: Vec::new(),
            current: None,
            end: 0,
            next: 0,
            matched: false,
        }
    }

    /// The next left row in order, sorting the left side first if it is not yet.
    fn next_left(&mut self) -> Option<Result<Row>> {
        if let Some(rows) = self.left_input.take() {
            let MergeSort {
                left_keys,
                desc,
                then,
                budget,
                ..
            } = &self.sort;
            match sort(rows, &self.left_fields, left_keys, desc, then, *budget) {
                Ok(sorted) => self.left = Some(sorted),
                Err(e) => return Some(Err(e)),
            }
        }
        self.left.as_mut()?.next()
    }

    /// How many right rows have the keys of `left`: they are the whole `group`.
    /// Left rows come in key order, so a right row with smaller keys than
    /// these matches no later left row either, and is dropped.
    fn find(&mut self, left: &Row) -> Result<usize> {
        let key = keys(&self.sort.left_keys, &self.left_fields, left)?;
        if key.iter().any(Value::is_null) {
            return Ok(0); // nothing is equal to NULL
        }
        if self
            .group_key
            .as_ref()
            .is_some_and(|group| compare(group, &key, &self.sort.desc).is_eq())
        {
            return Ok(self.group.len());
        }
        if let Some(rows) = self.right_input.take() {
            let MergeSort {
                right_keys,
                desc,
                budget,
                ..
            } = &self.sort;
            self.right = Some(sort(
                rows,
                &self.right_fields,
                right_keys,
                desc,
                &[],
                *budget,
            )?);
        }
        let right = self.right.as_mut().expect("sorted above");
        self.group.clear();
        loop {
            if self.lookahead.is_none() {
                let Some(row) = right.next() else { break };
                let row = row?;
                self.lookahead =
                    Some((keys(&self.sort.right_keys, &self.right_fields, &row)?, row));
            }
            // A right row with a NULL key is never equal, so never joins the group.
            let (right_key, _) = self.lookahead.as_ref().expect("read above");
            match compare(right_key, &key, &self.sort.desc) {
                Ordering::Less => self.lookahead = None,
                Ordering::Equal => self
                    .group
                    .push(self.lookahead.take().expect("read above").1),
                Ordering::Greater => break,
            }
        }
        self.group_key = Some(key);
        Ok(self.group.len())
    }
}

impl Iterator for MergeJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let row = match self.next_left()? {
                    Ok(row) => row,
                    Err(e) => return Some(Err(e)),
                };
                match self.find(&row) {
                    Ok(end) => self.end = end,
                    Err(e) => return Some(Err(e)),
                }
                (self.current, self.next, self.matched) = (Some(row), 0, false);
            }
            let left = self.current.as_ref().expect("set above");
            while self.next < self.end {
                let right = &self.group[self.next];
                self.next += 1;
                let joined: Row = left.iter().chain(right).cloned().collect();
                match matches_with(&self.condition, &self.fields, &joined, &mut *self.subqueries) {
                    Ok(true) => {
                        self.matched = true;
                        return Some(Ok(joined));
                    }
                    Ok(false) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            let mut left = self.current.take().expect("set above");
            if self.kind == JoinKind::Left && !self.matched {
                left.extend(std::iter::repeat_n(Value::Null, self.right_width));
                return Some(Ok(left));
            }
        }
    }
}

/// Sorts rows by the values of `keys`, in the order [`compare`] puts them
/// (which is how the sorter orders values of different types too), then by
/// `then`, as ORDER BY would.
fn sort(
    rows: Rows,
    fields: &[Field],
    keys: &[Expr],
    desc: &[bool],
    then: &[OrderBy],
    budget: usize,
) -> Result<Sorted> {
    let directions = desc
        .iter()
        .copied()
        .chain(then.iter().map(|o| o.desc))
        .collect();
    let mut sorter = Sorter::new(directions, None, budget);
    let mut sort_keys = Vec::new();
    for row in rows {
        let row = row?;
        sort_keys.clear();
        for e in keys {
            sort_keys.push(eval(e, fields, &row)?);
        }
        for o in then {
            sort_keys.push(eval(&o.expr, fields, &row)?);
        }
        sorter.push(&sort_keys, row)?;
    }
    sorter.finish()
}

/// Compares keys as a merge join sorts them: each in its direction, and the
/// values of one type all before or all after those of another, which they
/// are never equal to, but for INTs and REALs, which compare by value.
fn compare(a: &[Value], b: &[Value], desc: &[bool]) -> Ordering {
    a.iter()
        .zip(b)
        .zip(desc)
        .map(|((a, b), &desc)| {
            let ord = rank(a).cmp(&rank(b)).then_with(|| a.sort_cmp(b));
            if desc { ord.reverse() } else { ord }
        })
        .find(|ord| ord.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Where a value's type sorts among the others, NULL first.
fn rank(value: &Value) -> i64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Real(_) | Value::Decimal(_) => 2,
        Value::Text(_) => 3,
        Value::Blob(_) => 4,
        Value::Date(_) => 5,
        Value::TimestampTz(_) => 8,
        Value::Interval(_) => 9,
        Value::Time(_) => 6,
        Value::Timestamp(_) => 7,
    }
}

/// The values of the key expressions on a row.
fn keys(exprs: &[Expr], fields: &[Field], row: &Row) -> Result<Vec<Value>> {
    exprs.iter().map(|e| eval(e, fields, row)).collect()
}
