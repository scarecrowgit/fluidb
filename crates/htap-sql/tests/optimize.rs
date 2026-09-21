use std::collections::HashMap;

use htap_catalog::TableStats;
use htap_common::{ColumnDef, DataType, Value};
use htap_sql::expr::{BinOp, Expr};
use htap_sql::optimize::{
    extract_predicate_atoms, optimize, reorder_joins, validate_predicate_conservation,
    PhysicalQuery, StatsLookup,
};
use htap_sql::query::{BoundQuery, JoinKind, JoinTree, QueryBody, SelectBody, TableSlot};

struct MissingStats;

impl StatsLookup for MissingStats {
    fn table_stats(&self, _table: &str) -> Option<&TableStats> {
        None
    }
}

type InputTables = HashMap<usize, Vec<Vec<Value>>>;

#[derive(Clone, Debug, PartialEq)]
struct EvalRow {
    slots: Vec<Option<Vec<Value>>>,
}

impl EvalRow {
    fn leaf(slot_count: usize, slot: usize, values: Vec<Value>) -> Self {
        let mut slots = vec![None; slot_count];
        slots[slot] = Some(values);
        Self { slots }
    }

    fn merge(left: &Self, right: &Self) -> Self {
        let slots = left
            .slots
            .iter()
            .zip(&right.slots)
            .map(|(left, right)| left.clone().or_else(|| right.clone()))
            .collect();
        Self { slots }
    }

    fn null_extend(&self, slots: &[usize]) -> Self {
        let mut row = self.clone();
        for slot in slots {
            row.slots[*slot] = None;
        }
        row
    }

    fn flatten_logical(&self, select: &SelectBody) -> Vec<Value> {
        select
            .slots
            .iter()
            .enumerate()
            .flat_map(|(slot, table)| match &self.slots[slot] {
                Some(values) => values.clone(),
                None => vec![Value::Null; table.width()],
            })
            .collect()
    }
}

fn int(value: i64) -> Value {
    Value::Int64(value)
}

fn literal(value: i64) -> Expr {
    Expr::Literal(int(value))
}

fn column(slot: usize, column: usize, offset: usize, name: &str) -> Expr {
    Expr::ColumnRef {
        slot,
        column,
        offset,
        name: name.into(),
        data_type: DataType::Int64,
        nullable: true,
    }
}

fn logical_column(slot: usize, column_index: usize) -> Expr {
    column(slot, column_index, slot * 2 + column_index, "c")
}

fn eq(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        op: BinOp::Eq,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn gt(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        op: BinOp::Gt,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn and(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        op: BinOp::And,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn leaf(slot: usize) -> JoinTree {
    JoinTree::Leaf(slot)
}

fn join(kind: JoinKind, left: JoinTree, right: JoinTree, on: Option<Expr>) -> JoinTree {
    JoinTree::Join {
        kind,
        left: Box::new(left),
        right: Box::new(right),
        on,
    }
}

fn slots(tree: &JoinTree) -> Vec<usize> {
    fn collect(tree: &JoinTree, output: &mut Vec<usize>) {
        match tree {
            JoinTree::Leaf(slot) => output.push(*slot),
            JoinTree::Join { left, right, .. } => {
                collect(left, output);
                collect(right, output);
            }
        }
    }

    let mut output = Vec::new();
    collect(tree, &mut output);
    output
}

fn slot(table: &str) -> TableSlot {
    TableSlot::Base {
        table: table.into(),
        alias: table.into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
            },
            ColumnDef {
                name: "v".into(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: false,
            },
        ],
    }
}

fn select(relation_count: usize, join_tree: JoinTree, filter: Option<Expr>) -> SelectBody {
    SelectBody {
        slots: (0..relation_count)
            .map(|index| slot(&format!("t{index}")))
            .collect(),
        join_tree,
        visible_schemas: Vec::new(),
        filter,
        group_by: Vec::new(),
        aggregates: Vec::new(),
        windows: Vec::new(),
        having: None,
        projection: Vec::new(),
        distinct: false,
        correlated_outer_refs: Vec::new(),
    }
}

fn query(select: SelectBody) -> BoundQuery {
    BoundQuery {
        body: QueryBody::Select(select),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        subqueries: Vec::new(),
        correlated: false,
        correlated_outer_refs: Vec::new(),
        output_columns: Vec::new(),
    }
}

fn fixture_tables(relation_count: usize) -> InputTables {
    (0..relation_count)
        .map(|slot| {
            (
                slot,
                vec![
                    vec![int(1), int(slot as i64)],
                    vec![int(2), int(slot as i64 + 1)],
                    vec![int(2), int(slot as i64 + 2)],
                ],
            )
        })
        .collect()
}

fn value_at(row: &[Value], offset: usize) -> &Value {
    row.get(offset).unwrap_or(&Value::Null)
}

fn value_eq(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int32(left), Value::Int32(right)) => Some(left == right),
        (Value::Int64(left), Value::Int64(right)) => Some(left == right),
        (Value::Int32(left), Value::Int64(right)) => Some(i64::from(*left) == *right),
        (Value::Int64(left), Value::Int32(right)) => Some(*left == i64::from(*right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left == right),
        (Value::String(left), Value::String(right)) => Some(left == right),
        _ => Some(false),
    }
}

fn value_gt(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int32(left), Value::Int32(right)) => Some(left > right),
        (Value::Int64(left), Value::Int64(right)) => Some(left > right),
        (Value::Int32(left), Value::Int64(right)) => Some(i64::from(*left) > *right),
        (Value::Int64(left), Value::Int32(right)) => Some(*left > i64::from(*right)),
        _ => Some(false),
    }
}

fn eval(expr: &Expr, row: &[Value]) -> Value {
    match expr {
        Expr::Literal(value) => value.clone(),
        Expr::ColumnRef { offset, .. } => value_at(row, *offset).clone(),
        Expr::BinaryOp { op, left, right } => {
            let left = eval(left, row);
            let right = eval(right, row);
            match op {
                BinOp::Eq => value_eq(&left, &right)
                    .map(Value::Bool)
                    .unwrap_or(Value::Null),
                BinOp::Gt => value_gt(&left, &right)
                    .map(Value::Bool)
                    .unwrap_or(Value::Null),
                BinOp::And => match (left, right) {
                    (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
                    (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
                    _ => Value::Null,
                },
                other => panic!("unsupported test expression operator: {other:?}"),
            }
        }
        other => panic!("unsupported test expression: {other:?}"),
    }
}

fn predicate_is_true(expr: Option<&Expr>, row: &[Value]) -> bool {
    expr.is_none_or(|expr| eval(expr, row) == Value::Bool(true))
}

fn evaluate_tree(tree: &JoinTree, select: &SelectBody, tables: &InputTables) -> Vec<EvalRow> {
    match tree {
        JoinTree::Leaf(slot) => tables
            .get(slot)
            .into_iter()
            .flatten()
            .cloned()
            .map(|values| EvalRow::leaf(select.slots.len(), *slot, values))
            .collect(),
        JoinTree::Join {
            kind,
            left,
            right,
            on,
        } => {
            let left_rows = evaluate_tree(left, select, tables);
            let right_rows = evaluate_tree(right, select, tables);
            let right_slots = slots(right);
            let left_slots = slots(left);
            let mut output = Vec::new();
            let mut right_matched = vec![false; right_rows.len()];

            for left_row in &left_rows {
                let mut left_matched = false;
                for (right_index, right_row) in right_rows.iter().enumerate() {
                    let joined = EvalRow::merge(left_row, right_row);
                    let flat = joined.flatten_logical(select);
                    if predicate_is_true(on.as_ref(), &flat) {
                        left_matched = true;
                        right_matched[right_index] = true;
                        output.push(joined);
                    }
                }

                if !left_matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
                    output.push(left_row.null_extend(&right_slots));
                }
            }

            if matches!(kind, JoinKind::Right | JoinKind::Full) {
                for (matched, right_row) in right_matched.into_iter().zip(right_rows) {
                    if !matched {
                        output.push(right_row.null_extend(&left_slots));
                    }
                }
            }

            output
        }
    }
}

fn evaluate(select: &SelectBody, tables: &InputTables) -> Vec<Vec<Value>> {
    evaluate_tree(&select.join_tree, select, tables)
        .into_iter()
        .filter_map(|row| {
            let flat = row.flatten_logical(select);
            predicate_is_true(select.filter.as_ref(), &flat).then_some(flat)
        })
        .collect()
}

fn assert_shape(select: SelectBody, tables: &InputTables) {
    let atoms = extract_predicate_atoms(&select);
    let reordered = reorder_joins(&select, &MissingStats)
        .unwrap_or_else(|error| panic!("predicate conservation failed for {select:#?}: {error}"));
    validate_predicate_conservation(&select, &atoms, &reordered)
        .unwrap_or_else(|error| panic!("predicate conservation failed for {select:#?}: {error}"));

    let expected = evaluate(&select, tables);
    let physical = optimize(&query(select.clone()), &MissingStats);
    assert!(
        !physical.fallback,
        "optimizer unexpectedly fell back for {select:#?}"
    );
    let actual = evaluate_physical(&physical, tables);

    assert_eq!(
        row_multiset(&actual),
        row_multiset(&expected),
        "result multiset differs for {select:#?}"
    );

    // This query has no ORDER BY, so join reordering may legitimately change row
    // order. The multiset comparison above verifies the optimizer preserves all
    // rows and their multiplicities.
}

fn evaluate_physical(physical: &PhysicalQuery, tables: &InputTables) -> Vec<Vec<Value>> {
    let select = physical
        .select
        .as_ref()
        .expect("SELECT must produce a physical select");
    let mut logical_select = select.clone();

    // Physical column offsets follow the reordered join-tree layout, while this
    // evaluator materializes the final row in logical slot order. Normalize all
    // expressions back to logical offsets before evaluating the physical plan.
    fn normalize_expr(expr: &mut Expr, slots: &[TableSlot]) {
        match expr {
            Expr::ColumnRef {
                slot,
                column,
                offset,
                ..
            } => {
                *offset = slots[..*slot].iter().map(TableSlot::width).sum::<usize>() + *column;
            }
            Expr::BinaryOp { left, right, .. } => {
                normalize_expr(left, slots);
                normalize_expr(right, slots);
            }
            _ => {}
        }
    }

    fn normalize_tree(tree: &mut JoinTree, slots: &[TableSlot]) {
        if let JoinTree::Join {
            left, right, on, ..
        } = tree
        {
            normalize_tree(left, slots);
            normalize_tree(right, slots);
            if let Some(on) = on {
                normalize_expr(on, slots);
            }
        }
    }

    normalize_tree(&mut logical_select.join_tree, &logical_select.slots);
    if let Some(filter) = &mut logical_select.filter {
        normalize_expr(filter, &logical_select.slots);
    }

    evaluate(&logical_select, tables)
}

fn row_key(row: &[Value]) -> String {
    format!("{row:?}")
}

fn row_multiset(rows: &[Vec<Value>]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(row_key(row)).or_insert(0) += 1;
    }
    counts
}

#[test]
fn left_join_with_right_side_filter_in_on() {
    let on = and(
        eq(logical_column(0, 0), logical_column(1, 0)),
        gt(logical_column(1, 1), literal(1)),
    );
    let body = select(2, join(JoinKind::Left, leaf(0), leaf(1), Some(on)), None);
    assert_shape(body, &fixture_tables(2));
}

#[test]
fn left_join_with_right_side_filter_in_where() {
    let on = eq(logical_column(0, 0), logical_column(1, 0));
    let filter = gt(logical_column(1, 1), literal(1));
    let body = select(
        2,
        join(JoinKind::Left, leaf(0), leaf(1), Some(on)),
        Some(filter),
    );
    assert_shape(body, &fixture_tables(2));
}

#[test]
fn full_join_with_predicates_on_both_sides() {
    let on = and(
        eq(logical_column(0, 0), logical_column(1, 0)),
        and(
            gt(logical_column(0, 1), literal(-1)),
            gt(logical_column(1, 1), literal(0)),
        ),
    );
    let body = select(2, join(JoinKind::Full, leaf(0), leaf(1), Some(on)), None);
    assert_shape(body, &fixture_tables(2));
}

#[test]
fn inner_join_nested_under_left_join() {
    let inner = join(
        JoinKind::Inner,
        leaf(1),
        leaf(2),
        Some(eq(logical_column(1, 0), logical_column(2, 0))),
    );
    let outer = join(
        JoinKind::Left,
        leaf(0),
        inner,
        Some(eq(logical_column(0, 0), logical_column(1, 0))),
    );
    assert_shape(select(3, outer, None), &fixture_tables(3));
}

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn usize(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }

    fn bool(&mut self) -> bool {
        self.next() & 1 == 0
    }
}

fn bushy_tree(leaves: &[usize], rng: &mut Rng) -> JoinTree {
    if leaves.len() == 1 {
        return leaf(leaves[0]);
    }
    let split = 1 + rng.usize(leaves.len() - 1);
    join(
        JoinKind::Inner,
        bushy_tree(&leaves[..split], rng),
        bushy_tree(&leaves[split..], rng),
        None,
    )
}

fn conjunction(expressions: Vec<Expr>) -> Option<Expr> {
    expressions.into_iter().reduce(and)
}

fn generated_shape(seed: u64) -> (SelectBody, InputTables) {
    let mut rng = Rng(seed);
    let relation_count = 3 + rng.usize(4);
    let outer_count = rng.usize(3).min(relation_count - 2);
    let inner_count = relation_count - outer_count;
    let mut tree = bushy_tree(&(0..inner_count).collect::<Vec<_>>(), &mut rng);

    for slot in inner_count..relation_count {
        let kind = if rng.bool() {
            JoinKind::Left
        } else {
            JoinKind::Full
        };
        let on = and(
            eq(logical_column(0, 0), logical_column(slot, 0)),
            gt(logical_column(slot, 1), literal(-1)),
        );
        tree = if rng.bool() {
            join(kind, tree, leaf(slot), Some(on))
        } else {
            let reversed = match kind {
                JoinKind::Left => JoinKind::Right,
                other => other,
            };
            join(reversed, leaf(slot), tree, Some(on))
        };
    }

    let mut predicates = Vec::new();

    // NATURAL/USING binding produces ordinary equality expressions; generate the same
    // bound representation and include both connected and disconnected components.
    predicates.push(eq(logical_column(0, 0), logical_column(1, 0)));
    if inner_count >= 4 {
        predicates.push(eq(logical_column(2, 0), logical_column(3, 0)));
    }
    if inner_count == 3 || inner_count >= 5 {
        predicates.push(eq(
            logical_column(inner_count - 2, 0),
            logical_column(inner_count - 1, 0),
        ));
    }

    if rng.bool() {
        let left = rng.usize(inner_count);
        let mut right = rng.usize(inner_count);
        if right == left {
            right = (right + 1) % inner_count;
        }
        predicates.push(eq(logical_column(left, 1), logical_column(right, 1)));
    }
    if rng.bool() {
        predicates.push(gt(logical_column(rng.usize(inner_count), 1), literal(-1)));
    }

    let body = select(relation_count, tree, conjunction(predicates));
    (body, fixture_tables(relation_count))
}

#[test]
fn random_small_join_shapes_preserve_predicates_rows_and_order() {
    for seed in 0..192 {
        let (body, tables) = generated_shape(0x9e37_79b9_u64 ^ seed);
        assert_shape(body, &tables);
    }
}
