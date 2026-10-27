//! Join order: which of the tables of a FROM to join first.
//!
//! A query's inner joins may be done in any order, as `a JOIN b ON p JOIN c
//! ON q` keeps the same rows as `a JOIN c ON q JOIN b ON p` (the planner
//! puts each condition at the join that brings in the last table it
//! reads). The order makes a difference to the cost: joining first the
//! tables whose join keeps fewest rows keeps every later join small.
//!
//! ```text
//! FROM orders o, items i, users u                orders 1M, items 5M, users 1000
//! WHERE i.order_id = o.id AND o.user_id = u.id
//!   AND u.city = 'oslo'                          10 users in oslo
//!
//! as written:   (orders ⋈ items) ⋈ users         5M rows, then 50 thousand
//! cheapest:     (users ⋈ orders) ⋈ items         10 thousand, then 50 thousand
//! ```
//!
//! The planner builds left-deep trees: each join's right side is one table,
//! read once into a hash table or looked up through an index. Of those,
//! [`cheapest`] finds the one that costs least by dynamic programming over
//! the sets of tables, as System R did: the cheapest way to join a set is
//! the cheapest way to join one of its tables to the rest, whose own
//! cheapest way is already known. It never joins a table that no condition
//! links to those joined so far while there is one that is, so as not to
//! build cross products that a later condition would cut down. Over
//! [`EXHAUSTIVE`] tables, the sets are too many, and it adds tables
//! greedily instead: each time the one that is cheapest to join next.
//!
//! What a join costs and how many rows it returns are the caller's to say
//! (see `join_order` in `query.rs`): the methods the planner would pick, and
//! the estimates of `selectivity.rs`. Of plans that cost the same, the one
//! closest to the order written wins.

/// What reading a table, or joining one to those joined so far, gives: how
/// many rows come out, and what it costs, in rows read by a scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub rows: f64,
    pub cost: f64,
}

/// Up to this many tables are ordered by trying every set of them; more
/// are ordered greedily.
pub const EXHAUSTIVE: usize = 10;

/// The cheapest order in which to join `n` tables, and what it costs.
///
/// `first(t)` reads table `t` alone; `join(set, rows, t)` joins table `t` to
/// the tables of `set` (a bit for each), which return `rows` rows; and
/// `linked(set, t)` says whether a condition reads both `t` and a table of
/// `set`.
pub fn cheapest(
    n: usize,
    first: &dyn Fn(usize) -> Step,
    join: &dyn Fn(u64, f64, usize) -> Step,
    linked: &dyn Fn(u64, usize) -> bool,
) -> (Vec<usize>, f64) {
    assert!((1..64).contains(&n), "between 1 and 63 tables");
    if n > EXHAUSTIVE {
        return greedy(n, first, join, linked);
    }
    // For each set of tables, the cheapest way found to join them: its
    // cost, rows, and the table it joins last.
    let mut best: Vec<Option<(f64, f64, usize)>> = vec![None; 1 << n];
    for t in 0..n {
        let step = first(t);
        best[1 << t] = Some((step.cost, step.rows, t));
    }
    // A set's subsets are smaller numbers, so they are done before it.
    for set in 1..1u64 << n {
        if set.count_ones() < 2 {
            continue;
        }
        // The last table to join, from the last in FROM down, so that the
        // order written wins a tie.
        for t in (0..n).rev().filter(|&t| set & 1 << t != 0) {
            let rest = set & !(1 << t);
            let Some((cost, rows, _)) = best[rest as usize] else {
                continue;
            };
            let unlinked = !linked(rest, t);
            if unlinked && (0..n).any(|u| rest & 1 << u == 0 && linked(rest, u)) {
                continue; // a cross product, while a linked table waits
            }
            let step = join(rest, rows, t);
            let total = cost + step.cost;
            if best[set as usize].is_none_or(|(c, ..)| total < c) {
                best[set as usize] = Some((total, step.rows, t));
            }
        }
    }
    let all = (1u64 << n) - 1;
    let mut order = Vec::new();
    let mut set = all;
    while set != 0 {
        let (.., t) = best[set as usize].expect("every set is reached by some order");
        order.push(t);
        set &= !(1 << t);
    }
    order.reverse();
    (order, best[all as usize].expect("set above").0)
}

/// What joining tables in `order` costs, as [`cheapest`] counts it.
pub fn cost_of(
    order: &[usize],
    first: &dyn Fn(usize) -> Step,
    join: &dyn Fn(u64, f64, usize) -> Step,
) -> f64 {
    let Some((&t, rest)) = order.split_first() else {
        return 0.0;
    };
    let step = first(t);
    let (mut cost, mut rows, mut set) = (step.cost, step.rows, 1u64 << t);
    for &t in rest {
        let step = join(set, rows, t);
        cost += step.cost;
        rows = step.rows;
        set |= 1 << t;
    }
    cost
}

/// Orders tables greedily: first the one that returns fewest rows, then
/// each time the linked one that is cheapest to join next.
fn greedy(
    n: usize,
    first: &dyn Fn(usize) -> Step,
    join: &dyn Fn(u64, f64, usize) -> Step,
    linked: &dyn Fn(u64, usize) -> bool,
) -> (Vec<usize>, f64) {
    let start = (0..n)
        .min_by(|&a, &b| first(a).rows.total_cmp(&first(b).rows))
        .expect("at least one table");
    let step = first(start);
    let (mut order, mut cost, mut rows, mut set) =
        (vec![start], step.cost, step.rows, 1u64 << start);
    while order.len() < n {
        let left: Vec<usize> = (0..n).filter(|&t| set & 1 << t == 0).collect();
        let linked_ones: Vec<usize> = left.iter().copied().filter(|&t| linked(set, t)).collect();
        let candidates = if linked_ones.is_empty() {
            left
        } else {
            linked_ones
        };
        let (t, step) = candidates
            .into_iter()
            .map(|t| (t, join(set, rows, t)))
            .min_by(|(_, a), (_, b)| a.cost.total_cmp(&b.cost).then(a.rows.total_cmp(&b.rows)))
            .expect("a table is left");
        order.push(t);
        cost += step.cost;
        rows = step.rows;
        set |= 1 << t;
    }
    (order, cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain of tables, each linked to the next by an equality that keeps
    /// as many rows as the larger side, as a join on a key does; `rows`
    /// after their filters.
    fn chain(rows: &[f64]) -> (Vec<f64>, impl Fn(u64, usize) -> bool) {
        let n = rows.len();
        let linked = move |set: u64, t: usize| {
            (t > 0 && set & 1 << (t - 1) != 0) || (t + 1 < n && set & 1 << (t + 1) != 0)
        };
        (rows.to_vec(), linked)
    }

    fn plan(rows: &[f64], greedy_only: bool) -> (Vec<usize>, f64) {
        let (rows, linked) = chain(rows);
        let first = |t: usize| Step {
            rows: rows[t],
            cost: rows[t],
        };
        // A hash join reads the new table into a hash table, and probes it
        // with each row joined so far; the larger side's rows come out, or
        // every pair of a cross product.
        let join = |set: u64, left: f64, t: usize| Step {
            rows: if linked(set, t) {
                left.max(rows[t])
            } else {
                left * rows[t]
            },
            cost: 2.0 * rows[t] + 0.4 * left,
        };
        match greedy_only {
            true => greedy(rows.len(), &first, &join, &linked),
            false => cheapest(rows.len(), &first, &join, &linked),
        }
    }

    #[test]
    fn the_large_table_probes_and_the_small_ones_are_hashed() {
        // The cheapest reads the large table once and probes with it.
        let (order, cost) = plan(&[10.0, 1_000_000.0, 10.0], false);
        assert_eq!(order[0], 1, "{order:?}");
        assert_eq!(cost, 1_000_000.0 + (20.0 + 400_000.0) * 2.0);
    }

    #[test]
    fn ties_keep_the_order_written() {
        let (order, _) = plan(&[5.0, 5.0, 5.0, 5.0], false);
        assert_eq!(order, [0, 1, 2, 3]);
    }

    #[test]
    fn no_cross_product_while_a_linked_table_waits() {
        // Tables 0 and 2 are the smallest, but are linked only through 1.
        let (order, _) = plan(&[1.0, 100.0, 1.0], false);
        assert!(order[..2].contains(&1), "{order:?}");
    }

    #[test]
    fn exhaustive_search_costs_no_more_than_the_greedy_one() {
        let rows = [300.0, 2.0, 7000.0, 40.0, 5.0, 900.0, 60_000.0, 1.0];
        let (exhaustive, cost) = plan(&rows, false);
        let (greedy, greedy_cost) = plan(&rows, true);
        assert!(
            cost <= greedy_cost,
            "{exhaustive:?} {cost} {greedy:?} {greedy_cost}"
        );
        let (rows, _) = chain(&rows);
        let first = |t: usize| Step {
            rows: rows[t],
            cost: rows[t],
        };
        let (_, linked) = chain(&rows);
        let join = |set: u64, left: f64, t: usize| Step {
            rows: if linked(set, t) {
                left.max(rows[t])
            } else {
                left * rows[t]
            },
            cost: 2.0 * rows[t] + 0.4 * left,
        };
        assert_eq!(cost_of(&exhaustive, &first, &join), cost);
        assert_eq!(cost_of(&greedy, &first, &join), greedy_cost);
    }

    #[test]
    fn many_tables_are_ordered_greedily() {
        let rows: Vec<f64> = (0..EXHAUSTIVE + 2)
            .map(|i| ((i * 7919) % 101 + 1) as f64)
            .collect();
        let (order, _) = plan(&rows, false);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, (0..rows.len()).collect::<Vec<_>>());
    }
}
