//! Statistics: what `ANALYZE` learns about the values of a table's columns,
//! for the planner to estimate how many rows a condition keeps.
//!
//! ```text
//! ANALYZE orders                        rows 120000 (sampled 30000)
//!
//! column    nulls   distinct           most common values        histogram
//! user_id   0       3500               (none: no value stands      1 | 41 | 77 | ... | 3500
//!                                       out)                       32 buckets, 1/32 of the rest each
//! status    0       3                  'done' 0.81, 'open' 0.15,   (none: every value is common)
//!                                       'lost' 0.04
//! total     0.02    share 0.6 of rows  (none)                      0.5 | 3.2 | ... | 990.0
//! ```
//!
//! **Sampling.** ANALYZE reads every row, but keeps only a sample of
//! [`SAMPLE_ROWS`] of them, chosen uniformly by reservoir sampling
//! (Algorithm R): the first rows fill the reservoir, and row `i` then takes
//! a random place in it with probability `SAMPLE_ROWS / i`. The random
//! numbers come from a fixed seed, so analyzing the same rows gives the
//! same statistics. A table with no more rows than that is sampled whole,
//! and its statistics are exact.
//!
//! **Per column**, from the sample:
//!
//! - the share of NULLs;
//! - how many distinct values there are. A whole table's sample counts them.
//!   Otherwise a sample sees only some of them, and the Haas-Stokes
//!   estimator (as in PostgreSQL) scales its count up by how many values
//!   it saw only once, which hints at how many it did not see at all:
//!   `n d / (n - f1 + f1 n / N)`, for `d` distinct values in a sample of
//!   `n` of `N` rows, `f1` of them seen once. A count that comes out above
//!   a tenth of the rows is kept as a share of them, as such a column (an
//!   id, a timestamp) gains new values as rows are added, and the share
//!   follows the row count;
//! - the most common values ([`MOST_COMMON`] at most), each with the share
//!   of rows that hold it: those seen at least twice, and a quarter more
//!   often than the average value. If the sample is the whole table and has
//!   few enough distinct values, they all are;
//! - an equi-depth histogram of the other values: [`BUCKETS`] buckets at
//!   most, each holding as many of them, given by the values at their
//!   bounds. Where in its bucket a value falls is interpolated for numbers,
//!   and taken to be the middle for text.
//!
//! **Estimates.** A value among the most common keeps their share of rows;
//! another an equal part of what they leave, `(1 - nulls - common) /
//! (distinct - common values)`. A range keeps the common values in it, and
//! the share of the histogram below its upper bound less that below its
//! lower one. These assume the statistics are still about right: writes do
//! not change them, until the next ANALYZE. The planner takes the current
//! row count from the table (see [`Table::row_count`]).
//!
//! **Storage.** The statistics of a table are one row of the catalog (see
//! `catalog/mod.rs`), as text that [`TableStats::decode`] reads back. They
//! must fit in a page, so [`TableStats::shrink`] halves the histograms and
//! the lists of common values until they do, and a long text is never a
//! common value, and is cut short as a bound: `MAX_TEXT` characters
//! compare as the whole would, for all the histogram needs.

use std::cmp::Ordering;

use super::table::Table;
use crate::error::{Result, err};
use crate::predicate::Compare;
use crate::types::Value;

/// How many rows ANALYZE keeps as its sample, as PostgreSQL does by default.
pub const SAMPLE_ROWS: usize = 30_000;
/// The most common values kept per column, at most.
pub const MOST_COMMON: usize = 16;
/// The buckets of a column's histogram, at most.
pub const BUCKETS: usize = 32;
/// The characters of a text kept as a histogram bound; a longer text is
/// never a common value.
const MAX_TEXT: usize = 40;
/// A distinct count above this share of the rows is kept as a share.
const SHARE_ABOVE: f64 = 0.1;

/// What ANALYZE learned about a table's columns.
#[derive(Debug, Clone, PartialEq)]
pub struct TableStats {
    /// Rows the table had when it was analyzed.
    pub rows: u64,
    /// One for each column of the table, in order.
    pub columns: Vec<ColumnStats>,
}

/// What ANALYZE learned about the values of one column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// The share of rows whose value is NULL.
    pub nulls: f64,
    /// How many distinct values other than NULL there are.
    pub distinct: Distinct,
    /// The most common values, with the share of rows that hold each, most
    /// common first.
    pub common: Vec<(Value, f64)>,
    /// The bounds of an equi-depth histogram of the values that are neither
    /// NULL nor common, in order: `k + 1` values for `k` buckets, which hold
    /// as many of them each. Empty if there are none; a single value if
    /// they are all one value.
    pub bounds: Vec<Value>,
}

/// A count of distinct values, as a number or as a share of the rows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Distinct {
    Count(f64),
    Share(f64),
}

impl TableStats {
    /// Reads the rows of `table` and computes its statistics.
    pub fn collect(table: &Table) -> Result<TableStats> {
        let mut sample: Vec<Vec<Value>> = Vec::new();
        let mut rows: u64 = 0;
        let mut random = SplitMix(0x5EED);
        for item in table.scan() {
            let (_, row) = item?;
            rows += 1;
            if sample.len() < SAMPLE_ROWS {
                sample.push(row);
            } else {
                let at = random.below(rows);
                if at < SAMPLE_ROWS as u64 {
                    sample[at as usize] = row;
                }
            }
        }
        let whole = rows as usize == sample.len();
        let columns = (0..table.columns.len())
            .map(|c| {
                let values = sample.iter().map(|row| row[c].clone()).collect();
                ColumnStats::from_sample(values, rows, whole)
            })
            .collect();
        Ok(TableStats { rows, columns })
    }

    /// Halves each histogram and list of common values, for the statistics
    /// to take less room; false if there is nothing left to halve.
    pub fn shrink(&mut self) -> bool {
        let mut shrunk = false;
        for column in &mut self.columns {
            if !column.common.is_empty() {
                // The values dropped from the list are left to the histogram's
                // share, and spread over the values it does not list.
                column.common.truncate(column.common.len() / 2);
                shrunk = true;
            }
            if column.bounds.len() > 2 {
                let last = column.bounds.pop().expect("more than two");
                column.bounds = column.bounds.iter().step_by(2).cloned().collect();
                column.bounds.push(last);
                shrunk = true;
            } else if !column.bounds.is_empty() {
                column.bounds.clear();
                shrunk = true;
            }
        }
        shrunk
    }

    /// The statistics as text, for the catalog to keep:
    ///
    /// ```text
    /// rows 3 columns 2 | nulls 0 distinct count 3 common 0 bounds 2 i1 i3 | nulls 0.25 ...
    /// ```
    ///
    /// A value is `i` and an INT, `r` and a REAL, `b0` or `b1` for a BOOL,
    /// or `t`, the length of a TEXT in bytes, `:` and the TEXT.
    pub fn encode(&self) -> String {
        let mut out = format!("rows {} columns {}", self.rows, self.columns.len());
        for c in &self.columns {
            let distinct = match c.distinct {
                Distinct::Count(n) => format!("count {n:?}"),
                Distinct::Share(s) => format!("share {s:?}"),
            };
            out += &format!(" | nulls {:?} distinct {distinct}", c.nulls);
            out += &format!(" common {}", c.common.len());
            for (value, share) in &c.common {
                out += &format!(" {} {share:?}", encode_value(value));
            }
            out += &format!(" bounds {}", c.bounds.len());
            for value in &c.bounds {
                out += &format!(" {}", encode_value(value));
            }
        }
        out
    }

    /// Reads back what [`TableStats::encode`] wrote.
    pub fn decode(text: &str) -> Result<TableStats> {
        let mut r = Reader { text, at: 0 };
        r.expect("rows")?;
        let rows = r.number()? as u64;
        r.expect("columns")?;
        let count = r.number()? as usize;
        let mut columns = Vec::new();
        for _ in 0..count {
            r.expect("|")?;
            r.expect("nulls")?;
            let nulls = r.number()?;
            r.expect("distinct")?;
            let distinct = match r.word()? {
                "count" => Distinct::Count(r.number()?),
                "share" => Distinct::Share(r.number()?),
                other => return err(format!("corrupted statistics: {other}")),
            };
            r.expect("common")?;
            let mut common = Vec::new();
            for _ in 0..r.number()? as usize {
                common.push((r.value()?, r.number()?));
            }
            r.expect("bounds")?;
            let mut bounds = Vec::new();
            for _ in 0..r.number()? as usize {
                bounds.push(r.value()?);
            }
            columns.push(ColumnStats {
                nulls,
                distinct,
                common,
                bounds,
            });
        }
        if r.at < text.len() {
            return err("corrupted statistics: text after the last column");
        }
        Ok(TableStats { rows, columns })
    }
}

impl ColumnStats {
    /// The statistics of a column from the values of a sample of its rows,
    /// out of `rows` rows in all; `whole` if the sample is every row.
    pub fn from_sample(mut values: Vec<Value>, rows: u64, whole: bool) -> ColumnStats {
        let sampled = values.len();
        values.retain(|v| !v.is_null());
        if values.is_empty() {
            return ColumnStats {
                nulls: if sampled == 0 { 0.0 } else { 1.0 },
                distinct: Distinct::Count(0.0),
                common: Vec::new(),
                bounds: Vec::new(),
            };
        }
        let nulls = 1.0 - values.len() as f64 / sampled as f64;
        values.sort_by(Value::sort_cmp);

        // Each distinct value, and how many times the sample has it.
        let mut counts: Vec<(Value, usize)> = Vec::new();
        for value in values {
            match counts.last_mut() {
                Some((last, count)) if *last == value => *count += 1,
                _ => counts.push((value, 1)),
            }
        }
        let n = counts.iter().map(|(_, count)| count).sum::<usize>() as f64;
        let d = counts.len() as f64;
        let distinct = if whole {
            d
        } else {
            let seen_once = counts.iter().filter(|(_, count)| *count == 1).count() as f64;
            let total = (rows as f64 * (1.0 - nulls)).max(n);
            let estimate = n * d / (n - seen_once + seen_once * n / total);
            estimate.clamp(d, total)
        };
        let non_null_rows = rows as f64 * (1.0 - nulls);
        let distinct = if distinct > SHARE_ABOVE * non_null_rows && rows > 0 {
            Distinct::Share(distinct / rows as f64)
        } else {
            Distinct::Count(distinct)
        };

        // The common values: all of them if the whole table has few, else
        // those that stand out.
        let short = |v: &Value| !matches!(v, Value::Text(t) if t.chars().count() > MAX_TEXT);
        let mut common: Vec<(Value, usize)> = if whole && counts.len() <= MOST_COMMON {
            counts.iter().filter(|(v, _)| short(v)).cloned().collect()
        } else {
            let average = n / d;
            counts
                .iter()
                .filter(|(v, count)| *count >= 2 && *count as f64 > 1.25 * average && short(v))
                .cloned()
                .collect()
        };
        common.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.sort_cmp(&b.0)));
        common.truncate(MOST_COMMON);

        // The histogram of the rest, which are in order.
        let rest: Vec<&Value> = counts
            .iter()
            .filter(|(v, _)| !common.iter().any(|(c, _)| c == v))
            .flat_map(|(v, count)| std::iter::repeat_n(v, *count))
            .collect();
        let bounds = match rest.len() {
            0 => Vec::new(),
            m => {
                let buckets = BUCKETS.min(m - 1).max(1);
                match rest[0] == rest[m - 1] {
                    true => vec![cut(rest[0])],
                    false => (0..=buckets)
                        .map(|i| cut(rest[i * (m - 1) / buckets]))
                        .collect(),
                }
            }
        };
        let common = common
            .into_iter()
            .map(|(v, count)| (v, count as f64 / sampled as f64))
            .collect();
        ColumnStats {
            nulls,
            distinct,
            common,
            bounds,
        }
    }

    /// How many distinct values other than NULL the column has, in a table
    /// of `rows` rows: at least one, unless it is all NULLs.
    pub fn distinct(&self, rows: f64) -> f64 {
        let non_null = rows * (1.0 - self.nulls);
        let distinct = match self.distinct {
            Distinct::Count(n) => n,
            Distinct::Share(s) => s * rows,
        };
        if distinct <= 0.0 || non_null <= 0.0 {
            return 0.0;
        }
        distinct.min(non_null).max(non_null.min(1.0))
    }

    /// The share of rows the common values hold together.
    fn common_share(&self) -> f64 {
        self.common.iter().map(|(_, share)| share).sum()
    }

    /// The share of rows whose value equals `value`, in a table of `rows` rows.
    pub fn equal(&self, value: &Value, rows: f64) -> f64 {
        if value.is_null() || self.distinct(rows) == 0.0 {
            return 0.0;
        }
        if let Some((_, share)) = self
            .common
            .iter()
            .find(|(v, _)| v.compare(value).is_some_and(Ordering::is_eq))
        {
            return *share;
        }
        let rest = (1.0 - self.nulls - self.common_share()).max(0.0);
        let others = self.distinct(rows) - self.common.len() as f64;
        rest / others.max(1.0)
    }

    /// The share of rows whose value equals that of a row of some other
    /// column, whose values are not known: an equal part of the non-NULL
    /// rows for each distinct value.
    pub fn equal_any(&self, rows: f64) -> f64 {
        (1.0 - self.nulls) / self.distinct(rows).max(1.0)
    }

    /// The share of rows whose value is less than `value`.
    fn below(&self, value: &Value) -> f64 {
        let less = |v: &Value| v.compare(value).is_some_and(Ordering::is_lt);
        let common: f64 = self
            .common
            .iter()
            .filter(|(v, _)| less(v))
            .map(|(_, share)| share)
            .sum();
        let rest = (1.0 - self.nulls - self.common_share()).max(0.0);
        common + rest * self.histogram_below(value)
    }

    /// The share of the histogram's values that are less than `value`.
    fn histogram_below(&self, value: &Value) -> f64 {
        let less = |v: &Value| v.compare(value).is_some_and(Ordering::is_lt);
        let bounds = &self.bounds;
        // Bounds less than the value: the value is in the bucket after the last of them.
        let count = bounds.partition_point(|b| less(b));
        if count == 0 {
            return 0.0;
        }
        if count == bounds.len() {
            return 1.0;
        }
        let buckets = (bounds.len() - 1) as f64;
        let (low, high) = (&bounds[count - 1], &bounds[count]);
        let within = match (low.as_real(), high.as_real(), value.as_real()) {
            (Some(low), Some(high), Some(v)) if high > low => {
                ((v - low) / (high - low)).clamp(0.0, 1.0)
            }
            _ => 0.5,
        };
        (count as f64 - 1.0 + within) / buckets
    }

    /// The share of rows whose value is in a range: above `lower` and below
    /// `upper`, each `(value, inclusive)`; no bound for none.
    pub fn range(
        &self,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
        rows: f64,
    ) -> f64 {
        let under = match lower {
            None => 0.0,
            Some((v, true)) => self.below(v),
            Some((v, false)) => self.below(v) + self.equal(v, rows),
        };
        let up_to = match upper {
            None => 1.0 - self.nulls,
            Some((v, true)) => self.below(v) + self.equal(v, rows),
            Some((v, false)) => self.below(v),
        };
        (up_to - under).clamp(0.0, 1.0)
    }

    /// The share of rows for which `column op value` holds.
    pub fn compare(&self, op: Compare, value: &Value, rows: f64) -> f64 {
        if value.is_null() {
            return 0.0;
        }
        let share = match op {
            Compare::Eq => self.equal(value, rows),
            Compare::NotEq => 1.0 - self.nulls - self.equal(value, rows),
            Compare::Lt => self.range(None, Some((value, false)), rows),
            Compare::LtEq => self.range(None, Some((value, true)), rows),
            Compare::Gt => self.range(Some((value, false)), None, rows),
            Compare::GtEq => self.range(Some((value, true)), None, rows),
        };
        share.clamp(0.0, 1.0)
    }
}

/// A histogram bound: the value, or a long text's first characters.
fn cut(value: &Value) -> Value {
    match value {
        Value::Text(t) if t.chars().count() > MAX_TEXT => {
            Value::Text(t.chars().take(MAX_TEXT).collect())
        }
        value => value.clone(),
    }
}

fn encode_value(value: &Value) -> String {
    match value {
        Value::Int(n) => format!("i{n}"),
        Value::Real(r) => format!("r{r:?}"),
        Value::Bool(b) => format!("b{}", *b as u8),
        Value::Text(t) => format!("t{}:{t}", t.len()),
        Value::Null => unreachable!("statistics hold no NULL"),
    }
}

/// Reads the words and values of encoded statistics.
struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn word(&mut self) -> Result<&'a str> {
        let rest = self.text[self.at..].trim_start_matches(' ');
        let start = self.text.len() - rest.len();
        let len = rest.find(' ').unwrap_or(rest.len());
        if len == 0 {
            return err("corrupted statistics: ended early");
        }
        self.at = start + len;
        Ok(&self.text[start..start + len])
    }

    fn expect(&mut self, word: &str) -> Result<()> {
        match self.word()? {
            w if w == word => Ok(()),
            w => err(format!("corrupted statistics: {w} where {word} should be")),
        }
    }

    fn number(&mut self) -> Result<f64> {
        let word = self.word()?;
        word.parse()
            .map_err(|_| crate::error::Error(format!("corrupted statistics: {word}")))
    }

    fn value(&mut self) -> Result<Value> {
        let rest = self.text[self.at..].trim_start_matches(' ');
        let start = self.text.len() - rest.len();
        let bad = || err(format!("corrupted statistics at byte {start}"));
        if let Some(text) = rest.strip_prefix('t') {
            let Some((len, after)) = text.split_once(':') else {
                return bad();
            };
            let Ok(len) = len.parse::<usize>() else {
                return bad();
            };
            let Some(value) = after.get(..len) else {
                return bad();
            };
            self.at = start + 1 + text.len() - after.len() + len;
            return Ok(Value::Text(value.to_string()));
        }
        let word = self.word()?;
        let (tag, body) = word.split_at(1);
        Ok(match (tag, body) {
            ("i", n) => match n.parse() {
                Ok(n) => Value::Int(n),
                Err(_) => return bad(),
            },
            ("r", r) => match r.parse() {
                Ok(r) => Value::Real(r),
                Err(_) => return bad(),
            },
            ("b", "0") => Value::Bool(false),
            ("b", "1") => Value::Bool(true),
            _ => return bad(),
        })
    }
}

/// The SplitMix64 generator: random enough to sample rows, and the same
/// numbers every time.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
}

/// Counts how many times each value appears, for tests.
#[cfg(test)]
fn tally(values: &[Value]) -> std::collections::HashMap<Value, usize> {
    let mut counts = std::collections::HashMap::new();
    for v in values {
        *counts.entry(v.clone()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(values: impl IntoIterator<Item = i64>) -> Vec<Value> {
        values.into_iter().map(Value::Int).collect()
    }

    fn near(a: f64, b: f64, within: f64) -> bool {
        (a - b).abs() <= within
    }

    #[test]
    fn a_whole_table_gives_exact_counts_and_common_values() {
        // 0 four times, 1..=4 once each, and two NULLs.
        let mut values = ints([0, 0, 0, 0, 1, 2, 3, 4]);
        values.extend([Value::Null, Value::Null]);
        let stats = ColumnStats::from_sample(values, 10, true);
        assert!(near(stats.nulls, 0.2, 1e-9));
        assert_eq!(stats.distinct(10.0), 5.0);
        // Few enough distinct values for all of them to be common.
        assert_eq!(stats.common.len(), 5);
        assert_eq!(stats.common[0], (Value::Int(0), 0.4));
        assert_eq!(stats.equal(&Value::Int(0), 10.0), 0.4);
        assert_eq!(stats.equal(&Value::Int(3), 10.0), 0.1);
        assert_eq!(stats.equal(&Value::Null, 10.0), 0.0);
        assert!(near(
            stats.compare(Compare::Lt, &Value::Int(2), 10.0),
            0.5,
            1e-9
        ));
        assert!(near(
            stats.compare(Compare::GtEq, &Value::Int(2), 10.0),
            0.3,
            1e-9
        ));
        assert!(near(
            stats.compare(Compare::NotEq, &Value::Int(0), 10.0),
            0.4,
            1e-9
        ));
    }

    #[test]
    fn a_histogram_estimates_ranges_of_many_values() {
        // 0..1000, each once: no common values, a histogram of them all.
        let stats = ColumnStats::from_sample(ints(0..1000), 1000, true);
        assert!(stats.common.is_empty());
        assert_eq!(stats.bounds.len(), BUCKETS + 1);
        assert_eq!(stats.distinct, Distinct::Share(1.0));
        assert!(near(
            stats.compare(Compare::Lt, &Value::Int(250), 1000.0),
            0.25,
            0.01
        ));
        assert!(near(
            stats.compare(Compare::Gt, &Value::Int(900), 1000.0),
            0.1,
            0.01
        ));
        assert!(near(
            stats.range(
                Some((&Value::Int(100), true)),
                Some((&Value::Int(200), false)),
                1000.0
            ),
            0.1,
            0.01
        ));
        // Out of the histogram's range: none, or all.
        assert_eq!(stats.compare(Compare::Lt, &Value::Int(-5), 1000.0), 0.0);
        assert_eq!(stats.compare(Compare::LtEq, &Value::Int(5000), 1000.0), 1.0);
        // An equality: one row in a thousand, and as many when the table doubles.
        assert!(near(stats.equal(&Value::Int(7), 1000.0), 0.001, 1e-9));
        assert!(near(stats.equal(&Value::Int(7), 2000.0), 0.0005, 1e-9));
        // Numbers compare with numbers of the other type.
        assert!(near(
            stats.compare(Compare::Lt, &Value::Real(250.0), 1000.0),
            0.25,
            0.01
        ));
    }

    #[test]
    fn a_sample_of_a_large_table_estimates_how_many_distinct_values_it_has() {
        // A sample of 2000 rows, out of 100000, of values 0..500 each 200 times:
        // every value is seen several times, so there are no more.
        let mut random = SplitMix(1);
        let values: Vec<Value> = (0..2000)
            .map(|_| Value::Int(random.below(500) as i64))
            .collect();
        let distinct = tally(&values).len() as f64;
        let stats = ColumnStats::from_sample(values, 100_000, false);
        assert!(
            near(stats.distinct(100_000.0), 500.0, 25.0),
            "{:?}",
            stats.distinct
        );
        assert!(stats.distinct(100_000.0) >= distinct);
        // All distinct in the sample: likely a key, with as many values as rows.
        let stats = ColumnStats::from_sample(ints(0..2000), 100_000, false);
        assert_eq!(stats.distinct, Distinct::Share(1.0));
        assert_eq!(stats.distinct(100_000.0), 100_000.0);
    }

    #[test]
    fn skewed_values_are_common_and_the_rest_share_what_they_leave() {
        // 'a' in half of the rows, the other half 500 values, twice each.
        let mut values: Vec<Value> = vec![Value::Text("a".into()); 1000];
        values.extend((0..1000).map(|i| Value::Text(format!("v{}", i / 2))));
        let stats = ColumnStats::from_sample(values, 2000, true);
        assert_eq!(stats.common, vec![(Value::Text("a".into()), 0.5)]);
        assert!(near(
            stats.equal(&Value::Text("a".into()), 2000.0),
            0.5,
            1e-9
        ));
        assert!(near(
            stats.equal(&Value::Text("v7".into()), 2000.0),
            0.001,
            1e-6
        ));
        // Text in a bucket counts as its middle.
        let below = stats.compare(Compare::Lt, &Value::Text("v5".into()), 2000.0);
        assert!(below > 0.5 && below < 1.0, "{below}");
    }

    #[test]
    fn all_nulls_and_no_rows_have_no_values() {
        let stats = ColumnStats::from_sample(vec![Value::Null; 3], 3, true);
        assert_eq!(stats.nulls, 1.0);
        assert_eq!(stats.distinct(3.0), 0.0);
        assert_eq!(stats.compare(Compare::GtEq, &Value::Int(0), 3.0), 0.0);
        let stats = ColumnStats::from_sample(Vec::new(), 0, true);
        assert_eq!(stats.nulls, 0.0);
        assert_eq!(stats.equal(&Value::Int(1), 0.0), 0.0);
    }

    #[test]
    fn statistics_read_back_as_written_and_shrink() {
        let mut values = ints(0..100);
        values.extend([Value::Null]);
        let text = |s: &str| Value::Text(s.into());
        let words = vec![
            text("a b"),
            text("a b"),
            text("x:y |"),
            text(""),
            text("éé"),
            text("éé"),
        ];
        let stats = TableStats {
            rows: 101,
            columns: vec![
                ColumnStats::from_sample(values, 101, true),
                ColumnStats::from_sample(words, 6, true),
                ColumnStats::from_sample(
                    vec![Value::Real(-1.5), Value::Real(1e20), Value::Real(0.1)],
                    3,
                    true,
                ),
                ColumnStats::from_sample(
                    vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)],
                    3,
                    true,
                ),
                ColumnStats::from_sample(vec![Value::Int(i64::MIN), Value::Int(i64::MAX)], 2, true),
                ColumnStats::from_sample(vec![Value::Null], 1, true),
            ],
        };
        let encoded = stats.encode();
        assert_eq!(TableStats::decode(&encoded).unwrap(), stats, "{encoded}");

        let mut shrunk = stats.clone();
        assert!(shrunk.shrink());
        assert!(shrunk.encode().len() < encoded.len());
        assert_eq!(TableStats::decode(&shrunk.encode()).unwrap(), shrunk);
        while shrunk.shrink() {}
        assert!(
            shrunk
                .columns
                .iter()
                .all(|c| c.common.is_empty() && c.bounds.is_empty())
        );

        for bad in [
            "",
            "rows 1 columns 1",
            "rows 1 columns 0 extra",
            "rows x columns 0",
        ] {
            assert!(TableStats::decode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn long_texts_are_cut_as_bounds_and_never_common() {
        let long = "x".repeat(100);
        let values = vec![Value::Text(long.clone()); 4];
        let stats = ColumnStats::from_sample(values, 4, true);
        assert!(stats.common.is_empty());
        assert_eq!(stats.bounds, vec![Value::Text("x".repeat(MAX_TEXT))]);
    }
}
