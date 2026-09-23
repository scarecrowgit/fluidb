use std::collections::HashMap;
use std::time::Instant;

use htap_catalog::{CatalogSnapshot, StorageDescriptor, TableStats};
use htap_common::error::{HtapError, Result};
use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_sql::ast::BoundStatement;
use htap_sql::optimize::{
    optimize, BuildSide, EstimateSource, JoinNodeId, NodeEstimate, PhysicalQuery, StatsLookup,
};
use htap_sql::query::{BoundQuery, JoinTree, QueryBody, SelectBody, TableSlot};
use htap_sql::result::{CommandResult, StatementResult, StatementResult::Query};
use htap_sql::route::{classify_route, Route};

use crate::session::Principal;
use crate::{ExecMode, OwnedServer, VariableLookup};

struct CatalogStats<'a> {
    catalog: &'a CatalogSnapshot,
}

impl StatsLookup for CatalogStats<'_> {
    fn table_stats(&self, table: &str) -> Option<&TableStats> {
        self.catalog
            .table_by_name(table)
            .and_then(|descriptor| descriptor.stats.as_ref())
    }
}

struct PlanRow {
    node_id: u64,
    parent_id: Option<u64>,
    operation: String,
    table: Option<String>,
    estimated_rows: Option<u64>,
    estimate_source: Option<EstimateSource>,
    build_side: Option<BuildSide>,
}

pub(super) fn execute_explain(
    server: &OwnedServer,
    inner: BoundStatement,
    analyze: bool,
    catalog: &CatalogSnapshot,
    mode: &mut ExecMode<'_>,
    variables: &dyn VariableLookup,
    principal: &Principal,
) -> Result<StatementResult> {
    if matches!(inner, BoundStatement::Explain { .. }) {
        return Err(HtapError::Unsupported(
            "nested EXPLAIN statements are not supported".into(),
        ));
    }

    let route = route_for_statement(server, &inner, catalog)?;
    let mut plan = match (&inner, &route) {
        (BoundStatement::Select(select), Route::RowstorePointRead { .. }) => vec![PlanRow {
            node_id: 0,
            parent_id: None,
            operation: "RowstorePointRead".into(),
            table: Some(select.table.clone()),
            estimated_rows: None,
            estimate_source: None,
            build_side: None,
        }],
        (BoundStatement::AnalyticSelect(select), Route::OlapScan) => vec![PlanRow {
            node_id: 0,
            parent_id: None,
            operation: "OlapScan".into(),
            table: Some(select.table.clone()),
            estimated_rows: None,
            estimate_source: None,
            build_side: None,
        }],
        (BoundStatement::Query(query), Route::Query) => render_general_query(query, catalog),
        _ => vec![PlanRow {
            node_id: 0,
            parent_id: None,
            operation: operation_name(&route).into(),
            table: statement_table(&inner),
            estimated_rows: None,
            estimate_source: None,
            build_side: None,
        }],
    };

    let (actual_rows, actual_time_ms) = if analyze {
        let started = Instant::now();
        let result =
            server.dispatch_bound(inner, catalog, mode_for_nested(mode), variables, principal)?;
        let rows = result_row_count(&result);
        (Some(rows), Some(started.elapsed().as_millis() as u64))
    } else {
        (None, None)
    };

    // Runtime attribution is exact at the root. Child-node attribution remains best effort.
    let root_id = plan
        .iter()
        .find(|row| row.parent_id.is_none())
        .map(|row| row.node_id);
    Ok(render_rows(
        &mut plan,
        analyze,
        root_id,
        actual_rows,
        actual_time_ms,
    ))
}

fn mode_for_nested<'a>(mode: &'a mut ExecMode<'_>) -> ExecMode<'a> {
    match mode {
        ExecMode::Autocommit => ExecMode::Autocommit,
        ExecMode::Txn {
            snapshot,
            write_set,
        } => ExecMode::Txn {
            snapshot: *snapshot,
            write_set,
        },
    }
}

fn route_for_statement(
    server: &OwnedServer,
    statement: &BoundStatement,
    catalog: &CatalogSnapshot,
) -> Result<Route> {
    if matches!(
        statement,
        BoundStatement::CreateTable(_)
            | BoundStatement::DropTable(_)
            | BoundStatement::AlterPartitions(_)
            | BoundStatement::CreateUser(_)
            | BoundStatement::AlterUser(_)
            | BoundStatement::DropUser(_)
            | BoundStatement::GrantPrivileges(_)
            | BoundStatement::RevokePrivileges(_)
    ) {
        return Ok(Route::CatalogDdl);
    }

    let storage = match statement_table(statement) {
        Some(table_name) => {
            let table = catalog
                .table_by_name(&table_name)
                .ok_or_else(|| HtapError::NotFound(format!("table '{table_name}' not found")))?;
            table
                .partitions
                .first()
                .and_then(|partition| catalog.partition(*partition))
                .map(|partition| &partition.storage)
                .unwrap_or(&StorageDescriptor::Row)
        }
        None => &StorageDescriptor::Row,
    };

    let _ = server;
    classify_route(statement, storage)
}

fn statement_table(statement: &BoundStatement) -> Option<String> {
    match statement {
        BoundStatement::CreateTable(statement) => Some(statement.name.clone()),
        BoundStatement::Insert(statement) => Some(statement.table.clone()),
        BoundStatement::Delete(statement) => Some(statement.table.clone()),
        BoundStatement::Select(statement) => Some(statement.table.clone()),
        BoundStatement::AnalyticSelect(statement) => Some(statement.table.clone()),
        BoundStatement::AlterPartitions(statement) => Some(statement.table.clone()),
        BoundStatement::Update(statement) => Some(statement.table.clone()),
        BoundStatement::DropTable(statement) => Some(statement.table.clone()),
        BoundStatement::AnalyzeTable(table) => Some(table.clone()),
        BoundStatement::Query(query) => first_query_table(query),
        BoundStatement::Explain { .. }
        | BoundStatement::Show(_)
        | BoundStatement::CreateUser(_)
        | BoundStatement::AlterUser(_)
        | BoundStatement::DropUser(_)
        | BoundStatement::GrantPrivileges(_)
        | BoundStatement::RevokePrivileges(_)
        | BoundStatement::ShowGrants(_) => None,
    }
}

fn first_query_table(query: &BoundQuery) -> Option<String> {
    match &query.body {
        QueryBody::Select(select) => select.slots.iter().find_map(|slot| match slot {
            TableSlot::Base { table, .. } => Some(table.clone()),
            TableSlot::Derived { query, .. } => first_query_table(query),
            TableSlot::WorkingTableSlot { .. } => None,
        }),
        QueryBody::SetOp { left, right, .. } => {
            first_query_table(left).or_else(|| first_query_table(right))
        }
        QueryBody::RecursiveQueryBody {
            anchor,
            recursive_term,
            ..
        } => first_query_table(anchor).or_else(|| first_query_table(recursive_term)),
    }
}

fn render_general_query(query: &BoundQuery, catalog: &CatalogSnapshot) -> Vec<PlanRow> {
    let physical = optimize(query, &CatalogStats { catalog });
    let Some(select) = physical.select.as_ref() else {
        return vec![PlanRow {
            node_id: 0,
            parent_id: None,
            operation: "Query".into(),
            table: None,
            estimated_rows: None,
            estimate_source: None,
            build_side: None,
        }];
    };

    render_select_tree(select, &physical)
}

fn render_select_tree(select: &SelectBody, physical: &PhysicalQuery) -> Vec<PlanRow> {
    struct PlanVisitor<'a> {
        select: &'a SelectBody,
        estimates: &'a [NodeEstimate],
        build_sides: &'a HashMap<JoinNodeId, BuildSide>,
        next_id: u64,
        estimate_index: usize,
        join_index: usize,
        output: Vec<PlanRow>,
    }

    impl PlanVisitor<'_> {
        fn visit(&mut self, tree: &JoinTree, parent: Option<u64>) -> u64 {
            let node_id = self.next_id;
            self.next_id += 1;

            match tree {
                JoinTree::Leaf(slot) => {
                    let estimate = self.estimates.get(self.estimate_index).copied();
                    self.estimate_index += 1;
                    let table = self.select.slots.get(*slot).map(|slot| match slot {
                        TableSlot::Base { table, .. } => table.clone(),
                        TableSlot::Derived { .. } => "<derived>".into(),
                        TableSlot::WorkingTableSlot { .. } => "<working-table>".into(),
                    });
                    self.output.push(PlanRow {
                        node_id,
                        parent_id: parent,
                        operation: "TableScan".into(),
                        table,
                        estimated_rows: estimate.map(|estimate| estimate.row_count),
                        estimate_source: estimate.map(|estimate| estimate.source),
                        build_side: None,
                    });
                }
                JoinTree::Join {
                    kind, left, right, ..
                } => {
                    let row_position = self.output.len();
                    self.output.push(PlanRow {
                        node_id,
                        parent_id: parent,
                        operation: format!("{kind:?}Join"),
                        table: None,
                        estimated_rows: None,
                        estimate_source: None,
                        build_side: None,
                    });
                    self.visit(left, Some(node_id));
                    self.visit(right, Some(node_id));

                    let estimate = self.estimates.get(self.estimate_index).copied();
                    self.estimate_index += 1;
                    let current_join = self.join_index;
                    self.join_index += 1;
                    self.output[row_position].estimated_rows =
                        estimate.map(|estimate| estimate.row_count);
                    self.output[row_position].estimate_source =
                        estimate.map(|estimate| estimate.source);
                    self.output[row_position].build_side =
                        self.build_sides.get(&current_join).copied();
                }
            }

            node_id
        }
    }

    let mut visitor = PlanVisitor {
        select,
        estimates: &physical.node_estimates,
        build_sides: &physical.join_build_sides,
        next_id: 0,
        estimate_index: 0,
        join_index: 0,
        output: Vec::new(),
    };
    visitor.visit(&select.join_tree, None);
    visitor.output
}

fn operation_name(route: &Route) -> &'static str {
    match route {
        Route::CatalogDdl => "CatalogDdl",
        Route::RowstoreWrite => "RowstoreWrite",
        Route::RowstoreDelete { .. } => "RowstoreDelete",
        Route::RowstorePointRead { .. } => "RowstorePointRead",
        Route::OlapScan => "OlapScan",
        Route::Query => "Query",
        Route::RowstoreUpdate { .. } => "RowstoreUpdate",
        Route::CatalogRead => "CatalogRead",
        Route::Explain { .. } => "Explain",
    }
}

fn result_row_count(result: &StatementResult) -> u64 {
    match result {
        Query(result) => result.rows.len() as u64,
        StatementResult::Command(CommandResult::Dml { affected, .. }) => *affected,
        StatementResult::Command(CommandResult::Ddl { affected }) => *affected,
    }
}

fn render_rows(
    plan: &mut [PlanRow],
    analyze: bool,
    root_id: Option<u64>,
    actual_rows: Option<u64>,
    actual_time_ms: Option<u64>,
) -> StatementResult {
    let mut columns = vec![
        column("node_id", DataType::Int64, false),
        column("parent_id", DataType::Int64, true),
        column("operation", DataType::String, false),
        column("table", DataType::String, true),
        column("est_rows", DataType::Int64, true),
        column("estimate_source", DataType::String, true),
        column("build_side", DataType::String, true),
    ];
    if analyze {
        columns.push(column("actual_rows", DataType::Int64, true));
        columns.push(column("actual_time_ms", DataType::Int64, true));
    }

    let rows = plan
        .iter()
        .map(|node| {
            let mut values = vec![
                Value::Int64(node.node_id as i64),
                optional_u64(node.parent_id),
                Value::String(node.operation.clone()),
                optional_string(node.table.clone()),
                optional_u64(node.estimated_rows),
                optional_string(node.estimate_source.map(|source| match source {
                    EstimateSource::Stats => "Stats".to_string(),
                    EstimateSource::Default => "Default".to_string(),
                })),
                optional_string(node.build_side.map(|side| match side {
                    BuildSide::Left => "Left".to_string(),
                    BuildSide::Right => "Right".to_string(),
                })),
            ];
            if analyze {
                if Some(node.node_id) == root_id {
                    values.push(optional_u64(actual_rows));
                    values.push(optional_u64(actual_time_ms));
                } else {
                    values.push(Value::Null);
                    values.push(Value::Null);
                }
            }
            Row::new(values)
        })
        .collect();

    StatementResult::query(columns, rows)
}

fn column(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type,
        nullable,
        primary_key: false,
    }
}

fn optional_u64(value: Option<u64>) -> Value {
    value
        .map(|value| Value::Int64(value.min(i64::MAX as u64) as i64))
        .unwrap_or(Value::Null)
}

fn optional_string(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}
