//! `PLANTREE`: a plan drawn as a tree of relational algebra, the root at the
//! top and the tables at the bottom, each arrow pointing the way rows flow:
//!
//! ```text
//! PLANTREE SELECT r.id, s.cdate FROM r JOIN s ON r.id = s.id WHERE s.value > 100
//!
//!   ↑
//!   π r.id, s.cdate
//!   ↑
//!   ⋈ r.id = s.id (hash)
//! ┌─┴─┐
//! ↑   ↑
//! │   σ s.value > 100
//! │   ↑
//! r   s
//! ```
//!
//! It draws the plan `EXPLAIN` lists, step for step, so it shows what runs:
//! the join methods, filters pushed down to their tables, and the index or
//! key a table is read through, in brackets after its name.
//!
//! | step                   | drawn as                          |
//! |------------------------|-----------------------------------|
//! | a table's scan         | its name and alias, `[index i]` if read through one |
//! | filter                 | `σ condition`                     |
//! | projection             | `π expressions`                   |
//! | join                   | `⋈ condition (method)`; `⟕` for LEFT JOIN, `×` with no condition |
//! | aggregate              | `γ keys; aggregates`              |
//! | sort                   | `τ keys`, `(top n)` with a limit  |
//! | distinct               | `δ`                               |
//! | union                  | `∪`                               |
//! | subquery in FROM       | `ρ alias`                         |
//! | limit, gather          | `limit n offset m`, `gather (n workers)` |
//!
//! **Layout.** Each subtree is drawn into a block of rows, with the column
//! its root's arrow leaves from, its anchor. A step goes above its inputs'
//! blocks, set side by side, with its symbol over the middle of their
//! anchors; a line joins them when there are several. The inputs' blocks
//! end on one row, the shorter ones' arrows drawn longer, so the tables are
//! all at the bottom.

use crate::plan::query::{JoinMethod, LookupKey, Node, Phase, aggregate_name, label, sort_key};
use crate::result::width;
use crate::sql::ast::{Expr, JoinKind, SetOp};
use crate::types::StorageKind;

/// Columns between the blocks of a step's inputs.
const GAP: usize = 3;

/// The plan drawn as a tree, the arrow of its result on top.
pub fn draw(root: &Node) -> String {
    let block = block(root);
    let mut rows = vec![at(&[(block.anchor, "↑")])];
    rows.extend(block.rows);
    rows.iter()
        .map(|row| row.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A drawn subtree: its rows, and the column the arrow above it leaves from.
struct Block {
    rows: Vec<String>,
    anchor: usize,
}

/// What a step is drawn as: its text, and the column of the text that goes
/// over its inputs' arrows (its symbol, or the middle of a table's name).
struct Label {
    text: String,
    anchor: usize,
}

impl Label {
    /// An operator: its symbol first, over the arrows.
    fn op(text: String) -> Label {
        Label { text, anchor: 0 }
    }

    /// A table, the arrow leaving from the middle of its name.
    fn table(name: String) -> Label {
        let anchor = width(name.split(' ').next().unwrap_or("")) / 2;
        Label { text: name, anchor }
    }
}

fn block(node: &Node) -> Block {
    let (label, inputs) = step(node);
    let inputs: Vec<Block> = inputs.into_iter().map(block).collect();
    stack(label, inputs)
}

/// A step's label, and the steps it reads from that are drawn below it.
fn step<'n, 'a>(node: &'n Node<'a>) -> (Label, Vec<&'n Node<'a>>) {
    let list = |items: &mut dyn Iterator<Item = String>| items.collect::<Vec<_>>().join(", ");
    match node {
        Node::Scan {
            table,
            qualifier,
            access,
            predicates,
            ..
        } => {
            use crate::catalog::table::Access;
            let how = match access {
                Access::Scan
                    if table.storage_kind() == StorageKind::Column && !predicates.is_empty() =>
                {
                    " [compressed chunks filtered]".to_string()
                }
                Access::Scan => String::new(),
                Access::Key(_) => " [primary key]".into(),
                Access::KeyRange { .. } => " [primary key range]".into(),
                Access::Index {
                    index, covering, ..
                } => {
                    let index = &table.indexes()[*index];
                    let kind = if index.is_hash() {
                        "hash index"
                    } else {
                        "index"
                    };
                    let covering = if *covering { "covering " } else { "" };
                    format!(" [{covering}{kind} {}]", index.name)
                }
            };
            (
                Label::table(format!("{}{how}", label(table, qualifier))),
                vec![],
            )
        }
        Node::Lookup {
            table,
            qualifier,
            key,
            ..
        } => {
            // Looked up for each left row of the index join above.
            let through = match key {
                LookupKey::Index(i) if table.indexes()[*i].is_hash() => {
                    format!("hash index {}", table.indexes()[*i].name)
                }
                LookupKey::Index(i) => format!("index {}", table.indexes()[*i].name),
                LookupKey::PrimaryKey => "primary key".into(),
            };
            let name = format!("{} [{through}]", label(table, qualifier));
            (Label::table(name), vec![])
        }
        Node::Filter { input, predicate } => (Label::op(format!("σ {predicate}")), vec![input]),
        Node::Project { input, exprs } => {
            let exprs = list(&mut exprs.iter().map(Expr::to_string));
            (Label::op(format!("π {exprs}")), vec![input])
        }
        Node::Join {
            left,
            right,
            kind,
            method,
            condition,
        } => (
            Label::op(join(right, *kind, method, condition)),
            vec![left, right],
        ),
        Node::Aggregate {
            input,
            group_by,
            aggregates,
            phase,
        } => {
            let phase = match phase {
                Phase::Whole => "",
                Phase::Partial => "partial ",
                Phase::Final => "final ",
            };
            let aggregates = list(&mut aggregates.iter().map(aggregate_name));
            let text = match group_by.is_empty() {
                true => format!("γ {phase}{aggregates}"),
                false => {
                    let keys = list(&mut group_by.iter().map(Expr::to_string));
                    format!("γ {phase}{keys}; {aggregates}")
                }
            };
            (Label::op(text), vec![input])
        }
        Node::Sort { input, keys, limit } => {
            let keys = list(&mut keys.iter().map(|k| sort_key(&k.expr, k.desc)));
            let top = limit.map_or(String::new(), |n| format!(" (top {n})"));
            (Label::op(format!("τ {keys}{top}")), vec![input])
        }
        Node::Limit {
            input,
            count,
            offset,
        } => {
            let text = match (count, offset) {
                (Some(count), 0) => format!("limit {count}"),
                (Some(count), offset) => format!("limit {count} offset {offset}"),
                (None, offset) => format!("offset {offset}"),
            };
            (Label::op(text), vec![input])
        }
        Node::Distinct { input } => (Label::op("δ".into()), vec![input]),
        Node::Union { inputs } => (Label::op("∪".into()), inputs.iter().collect()),
        Node::Derived {
            input, qualifier, ..
        } => (Label::op(format!("ρ {qualifier}")), vec![input]),
        Node::Gather { input, workers } => (
            Label::op(format!("gather ({workers} workers)")),
            vec![input],
        ),
        Node::Values { rows, .. } => {
            let rows = match rows.len() {
                1 => "1 row".to_string(),
                n => format!("{n} rows"),
            };
            (Label::table(format!("values ({rows})")), vec![])
        }
        Node::WorkTable { qualifier, .. } => {
            (Label::table(format!("{qualifier} [work table]")), vec![])
        }
        Node::Recursive {
            anchor, step, all, ..
        } => {
            let all = if *all { " all" } else { "" };
            (Label::op(format!("∪ recursive{all}")), vec![anchor, step])
        }
        Node::SetOp {
            left,
            right,
            op,
            all,
        } => {
            let symbol = match op {
                SetOp::Union => "∪",
                SetOp::Intersect => "∩",
                SetOp::Except => "−",
            };
            let all = if *all { " all" } else { "" };
            (Label::op(format!("{symbol}{all}")), vec![left, right])
        }
        Node::Window {
            input, functions, ..
        } => {
            let functions = list(&mut functions.iter().map(|w| w.to_string()));
            (Label::op(format!("ω {functions}")), vec![input])
        }
        // Its query is planned again for each left row, so only the left
        // side is drawn below it.
        Node::Lateral {
            left,
            qualifier,
            kind,
            condition,
            ..
        } => {
            let on = condition
                .as_ref()
                .map_or(String::new(), |c| format!(" {c}"));
            let symbol = join_symbol(*kind, false);
            (
                Label::op(format!(
                    "{symbol} lateral {qualifier}{on} (planned for each left row)"
                )),
                vec![left],
            )
        }
    }
}

/// A join's label: its symbol, what it joins on, and how.
fn join(right: &Node, kind: JoinKind, method: &JoinMethod, condition: &Option<Expr>) -> String {
    let mut on: Vec<String> = match method {
        JoinMethod::Hash {
            left_keys,
            right_keys,
            ..
        }
        | JoinMethod::Merge {
            left_keys,
            right_keys,
            ..
        } => left_keys
            .iter()
            .zip(right_keys)
            .map(|(l, r)| format!("{l} = {r}"))
            .collect(),
        // The equality an index join looks rows up by is its right side's.
        JoinMethod::Index => lookup_of(right)
            .map(|(left_key, column)| format!("{left_key} = {column}"))
            .into_iter()
            .collect(),
        JoinMethod::Block | JoinMethod::Naive => Vec::new(),
    };
    on.extend(condition.iter().map(Expr::to_string));
    let how = match method {
        JoinMethod::Hash { workers, .. } if *workers > 1 => {
            format!("parallel hash, {workers} workers")
        }
        JoinMethod::Hash { .. } => "hash".into(),
        JoinMethod::Index => "index".into(),
        JoinMethod::Merge { .. } => "merge".into(),
        JoinMethod::Block => "nested loop".into(),
        JoinMethod::Naive => "naive nested loop".into(),
    };
    let symbol = join_symbol(kind, on.is_empty());
    match on.is_empty() {
        true => format!("{symbol} ({how})"),
        false => format!("{symbol} {} ({how})", on.join(" AND ")),
    }
}

/// A join's symbol: `⋈`, or `×` for an inner join on nothing; `⟕`, `⟖` or
/// `⟗` for a left, right or full outer join.
fn join_symbol(kind: JoinKind, cross: bool) -> &'static str {
    match (kind, cross) {
        (JoinKind::Left, _) => "⟕",
        (JoinKind::Right, _) => "⟖",
        (JoinKind::Full, _) => "⟗",
        (JoinKind::Inner, true) => "×",
        (JoinKind::Inner, false) => "⋈",
    }
}

/// The left key and the column an index join's lookup finds rows by,
/// below the filters on top of it.
fn lookup_of(node: &Node) -> Option<(String, String)> {
    match node {
        Node::Filter { input, .. } => lookup_of(input),
        Node::Lookup {
            table,
            qualifier,
            column,
            left_key,
            ..
        } => Some((
            left_key.to_string(),
            format!("{qualifier}.{}", table.columns[*column].name),
        )),
        _ => None,
    }
}

/// A step drawn above the blocks of its inputs.
fn stack(label: Label, inputs: Vec<Block>) -> Block {
    if inputs.is_empty() {
        return Block {
            rows: vec![label.text],
            anchor: label.anchor,
        };
    }
    // The inputs side by side, ending on one row: a shorter one's arrow
    // goes on down to it.
    let height = inputs.iter().map(|b| b.rows.len()).max().unwrap_or(0);
    let mut lefts = Vec::new();
    let mut x = 0;
    for input in &inputs {
        lefts.push(x);
        x += input
            .rows
            .iter()
            .map(|r| width(r))
            .max()
            .unwrap_or(0)
            .max(input.anchor + 1)
            + GAP;
    }
    let mut below = Vec::new();
    for row in 0..height {
        let parts: Vec<(usize, &str)> = inputs
            .iter()
            .zip(&lefts)
            .map(
                |(input, &left)| match row.checked_sub(height - input.rows.len()) {
                    Some(i) => (left, input.rows[i].as_str()),
                    None => (left + input.anchor, "│"),
                },
            )
            .collect();
        below.push(at(&parts));
    }
    let anchors: Vec<usize> = inputs
        .iter()
        .zip(&lefts)
        .map(|(input, left)| left + input.anchor)
        .collect();
    let (first, last) = (anchors[0], anchors[anchors.len() - 1]);
    let middle = (first + last) / 2;

    // Room on the left for the label, if it starts left of every input.
    let shift = label.anchor.saturating_sub(middle);
    let (anchors, middle) = (
        anchors.iter().map(|a| a + shift).collect::<Vec<_>>(),
        middle + shift,
    );
    let pad = " ".repeat(shift);

    let mut rows = vec![at(&[(middle - label.anchor, &label.text)])];
    if anchors.len() > 1 {
        rows.push(line(&anchors, middle));
    }
    let arrows: Vec<(usize, &str)> = anchors.iter().map(|&a| (a, "↑")).collect();
    rows.push(at(&arrows));
    rows.extend(below.into_iter().map(|row| format!("{pad}{row}")));
    Block {
        rows,
        anchor: middle,
    }
}

/// A row with each text at its column, in order of their columns: the
/// space before each one filled.
fn at(parts: &[(usize, &str)]) -> String {
    let mut row = String::new();
    for &(column, text) in parts {
        let used = width(&row);
        row.push_str(&" ".repeat(column.saturating_sub(used)));
        row.push_str(text);
    }
    row
}

/// The line from the first input's arrow to the last's, with a branch down
/// to each and one up to the step at `middle`.
fn line(anchors: &[usize], middle: usize) -> String {
    let (first, last) = (anchors[0], anchors[anchors.len() - 1]);
    (first..=last)
        .map(|column| {
            let down = anchors.contains(&column);
            match (column, down, column == middle) {
                (c, _, _) if c == first => '┌',
                (c, _, _) if c == last => '┐',
                (_, true, true) => '┼',
                (_, true, false) => '┬',
                (_, false, true) => '┴',
                (_, false, false) => '─',
            }
        })
        .fold(" ".repeat(first), |mut row, c| {
            row.push(c);
            row
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(name: &str) -> Block {
        stack(Label::table(name.into()), Vec::new())
    }

    fn drawn(block: Block) -> String {
        block
            .rows
            .iter()
            .map(|r| r.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_step_goes_over_its_input_s_arrow() {
        let filter = stack(Label::op("σ value > 100".into()), vec![leaf("s")]);
        assert_eq!(drawn(filter), "σ value > 100\n↑\ns");
        // A table's arrow leaves from the middle of its name.
        let filter = stack(Label::op("σ x".into()), vec![leaf("orders o")]);
        assert_eq!(drawn(filter), "   σ x\n   ↑\norders o");
    }

    #[test]
    fn a_join_joins_its_inputs_arrows_and_the_tables_end_at_the_bottom() {
        let right = stack(Label::op("σ value > 100".into()), vec![leaf("s")]);
        let join = stack(Label::op("⋈ r.id = s.id".into()), vec![leaf("r"), right]);
        let project = stack(Label::op("π r.id, s.cdate".into()), vec![join]);
        assert_eq!(
            drawn(project),
            [
                "  π r.id, s.cdate",
                "  ↑",
                "  ⋈ r.id = s.id",
                "┌─┴─┐",
                "↑   ↑",
                "│   σ value > 100",
                "│   ↑",
                "r   s",
            ]
            .join("\n")
        );
    }

    #[test]
    fn three_inputs_branch_down_to_each() {
        let union = stack(Label::op("∪".into()), vec![leaf("a"), leaf("b"), leaf("c")]);
        assert_eq!(drawn(union), "    ∪\n┌───┼───┐\n↑   ↑   ↑\na   b   c");
        let union = stack(
            Label::op("∪".into()),
            vec![leaf("a"), leaf("b"), leaf("c"), leaf("d")],
        );
        assert_eq!(
            drawn(union),
            "      ∪\n┌───┬─┴─┬───┐\n↑   ↑   ↑   ↑\na   b   c   d"
        );
    }

    #[test]
    fn a_label_wider_on_the_left_than_its_inputs_moves_them_right() {
        let label = Label {
            text: "students s".into(),
            anchor: 4,
        };
        let block = stack(label, vec![leaf("t")]);
        assert_eq!(drawn(block), "students s\n    ↑\n    t");
    }
}
