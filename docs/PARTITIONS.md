# Partition System Architecture and Execution Guide

This document provides a code-grounded, comprehensive architectural guide to the partition subsystem in HTAP. It details the creation boundaries, catalog metadata models, routing and local topology invariants, transactional DML execution, analytical query pruning and parallel scans, physical storage format interactions, crash recovery semantics (qualified as tested process-crash/reopen and atomic publication behavior; no power-loss/fsync proof), and deferred distributed features.

---

## 1. Scope and Creation Boundary

Partitioning in the local HTAP engine separates native internal administrative topology configuration from the user-facing SQL parsing interface.

```text
┌────────────────────────────────────────────────────────────────────────┐
│ SQL Interface (sqlparser 0.62 / MySqlDialect)                          │
│                                                                        │
│   CREATE TABLE ...                 --> Unpartitioned table (p0)        │
│   CREATE TABLE ... PARTITION BY .. --> REJECTED (HtapError::InvalidArg)│
└──────────────────────────────────┬─────────────────────────────────────┘
                                   │
┌──────────────────────────────────▼─────────────────────────────────────┐
│ Native Administrative API (LocalServer)                                │
│                                                                        │
│   LocalServer::create_partitioned_table(PartitionedTableDefinition)    │
│     ├── PartitionTopology::Range (finite [lower, upper) intervals)    │
│     └── PartitionTopology::List  (finite disjoint value sets)          │
└────────────────────────────────────────────────────────────────────────┘
```

### SQL DDL: Default Single-Partition (`p0`) Behavior

Standard SQL DDL executed through `LocalServer::execute` or `LocalServer::execute_create_table` creates an **unpartitioned** table. In the catalog model:
- `TableDescriptor.partitioning` is set to `None`.
- The table contains exactly one default partition named `"p0"`.
- The partition is assigned a single tablet (`bucket = 0`) and a single local leader replica on node 1 (`NodeId(1)`).
- All DML mutations and queries route to this single partition automatically.

### Native Administrative API: Finite Range and List Partitioning

Partitioned tables are defined exclusively via the native in-process API:
```rust
pub fn create_partitioned_table(
    &self,
    definition: PartitionedTableDefinition,
) -> Result<StatementResult>
```

`PartitionedTableDefinition` (`crates/htap-server/src/lib.rs`) requires:
- `name: String`: Unique logical table name.
- `schema: Schema`: Non-empty schema column definitions.
- `primary_key: Vec<usize>`: Column indices forming the primary key.
- `topology: PartitionTopology`: Explicit topology definition, supporting two finite variants:
  1. `PartitionTopology::Range { key_column: usize, partitions: Vec<RangePartitionDefinition> }`:
     Each `RangePartitionDefinition` defines a partition name and a half-open interval `[lower, upper)`.
  2. `PartitionTopology::List { key_column: usize, partitions: Vec<ListPartitionDefinition> }`:
     Each `ListPartitionDefinition` defines a partition name and a non-empty set of explicit values.

**Empty Topology Rejection:**
Attempting to create a partitioned table with an empty partition vector (`partitions.is_empty()`) is rejected before acquiring the execution lock, mutating the catalog, or allocating IDs, returning `HtapError::InvalidArgument` (`test_partitioned_empty_topology_rejection_no_catalog_mutation` in `crates/htap-server/tests/local_server.rs`).

### Rejection of MySQL Partition DDL

MySQL partitioning syntax (`PARTITION BY RANGE (...)`, `PARTITION BY RANGE COLUMNS (...)`, `PARTITION BY LIST (...)`, `PARTITION BY LIST COLUMNS (...)`, and `PARTITION BY ... VALUES LESS THAN MAXVALUE`) is intentionally **not** supported via SQL DDL:
- **Parser Level (`sqlparser 0.62` / `MySqlDialect`):** The pinned parser grammar does not retain MySQL partition definitions in its AST. All MySQL partition DDL strings fail during `parse_one` and return `HtapError::InvalidArgument("SQL parse error: ...")`. Verified in `crates/htap-sql/tests/parse_bind.rs`: `test_mysql_partition_ddl_rejected_at_parser_level`.
- **Binder Level:** If a `Statement::CreateTable` AST node is manually constructed with `partition_by = Some(...)`, `htap-sql::bind` (`crates/htap-sql/src/binder.rs:172`) rejects it with `HtapError::Unsupported("ORDER BY / PARTITION BY / CLUSTER BY not supported in CREATE TABLE")`. Verified via manual-AST rejection in `crates/htap-sql/tests/parse_bind.rs:979-986`: `test_negative_create_table`.
- No lossy regex pre-parsing or coercion of unrelated AST fields is permitted (ADR-011).

---

## 2. Catalog Model and Strict Validation

The catalog (`htap-catalog`) models partitioned tables hierarchically:
`TableDescriptor` -> `PartitionDescriptor` -> `TabletDescriptor` -> `ReplicaDescriptor`.

```text
TableDescriptor
├── id: TableId
├── name: String
├── schema: Schema
├── primary_key: Vec<usize>
├── partitions: Vec<PartitionId>  [preserves deterministic sequence order]
├── generation: u64
└── partitioning: Option<PartitioningDescriptor>
      ├── key_column: usize
      └── method: PartitioningMethod (Range | List)
            │
            ├── Range: PartitionDescriptor.range = Some(RangeBound { lower, upper })
            └── List:  PartitionDescriptor.list_values = Vec<Value>
```

### Entity Descriptors (`crates/htap-catalog/src/model.rs`)

1. **`TableDescriptor`:**
   - Holds table identity, schema, primary key indices, schema generation, and `partitioning: Option<PartitioningDescriptor>`.
   - `partitions: Vec<PartitionId>` explicitly defines the deterministic partition sequence used for routing, scanning, and merging.
2. **`PartitioningDescriptor`:**
   - `key_column: usize`: Zero-based column index in `schema`.
   - `method: PartitioningMethod`: `PartitioningMethod::Range` or `PartitioningMethod::List`.
3. **`PartitionDescriptor`:**
   - `id: PartitionId`, `table_id: TableId`, `name: String`.
   - `storage: StorageDescriptor`: Current storage format (`Row`, `Column`, or `Converting { from, to, generation }`).
   - `tablets: Vec<TabletId>`: Tablets belonging to this partition.
   - `range: Option<RangeBound>`: Half-open bound `[lower, upper)` for range partitioning.
   - `list_values: Vec<Value>`: Explicit value list for list partitioning.
   - `conversion: Option<ConversionDescriptor>`: In-flight conversion state if converting.
4. **`RangeBound`:**
   - `lower: Value`: Inclusive lower bound.
   - `upper: Value`: Exclusive upper bound (`lower <= value < upper`).
5. **`TabletDescriptor`:**
   - `id: TabletId`, `partition_id: PartitionId`, `bucket: u32`.
   - `replicas: Vec<ReplicaId>`: Replicas of this tablet.
   - `column_manifest: Option<ColumnManifestRef>`: Reference to columnar manifest (`HTAPTBM1`) if in `Column` or `Converting` storage.
6. **`ReplicaDescriptor`:**
   - `id: ReplicaId`, `tablet_id: TabletId`, `node_id: NodeId`.
   - `is_leader: bool`, `healthy: bool`, `generation: u64`.

### Catalog Validation Rules (`CatalogSnapshot::validate()`)

Every catalog snapshot must pass strict integrity validation before being committed or loaded on reopen:

1. **Partition Key Constraints:**
   - `partitioning.key_column` must be within schema bounds (`key_column < schema.len()`).
   - **Key in Primary Key:** The partition key must be an explicit member of the primary key (`table.primary_key.contains(&key_column)`). Tables with partition keys outside the PK are rejected.
   - **Non-Null Constraint:** The partition key column cannot be nullable (`!key_col.nullable`).
   - A partitioned table must declare at least one partition (`!table.partitions.is_empty()`).
2. **Bidirectional Ownership Invariants:**
   - Tables reference partitions, and every referenced partition must reference the table back (`part.table_id == table.id`).
   - No orphan partitions, duplicate partition IDs, or partition name collisions within a table.
   - Partitions reference tablets, and tablets reference back (`tablet.partition_id == part.id`).
   - Tablets reference replicas, and replicas reference back (`replica.tablet_id == tablet.id`).
3. **Range Partition Validation:**
   - Lower bound must be strictly less than upper bound (`range.lower < range.upper`).
   - Bound data types must match the partition key column type (`range.lower.data_type() == Some(expected_type)` and `range.upper.data_type() == Some(expected_type)`).
   - Bounds cannot be null.
   - **Non-Overlapping Ranges:** Partitions within the same table cannot overlap. Overlap is checked pairwise via `std::cmp::max(l1, l2) < std::cmp::min(u1, u2)`. Any overlap returns `HtapError::InvalidArgument`.
   - Partitions in range tables must have `range.is_some()` and empty `list_values`.
4. **List Partition Validation:**
   - List values vector cannot be empty (`!part.list_values.is_empty()`).
   - All values must match the partition key column type and cannot be null.
   - **Uniqueness & Disjointness:** List values must be unique within each partition and mutually disjoint across all partitions of the table (no duplicate values allowed).
   - Partitions in list tables must have empty `range` and non-empty `list_values`.
5. **Unpartitioned Table Consistency:**
   - Partitions belonging to unpartitioned tables (`table.partitioning == None`) must have `range.is_none()` and empty `list_values`.
6. **Storage & Conversion Consistency:**
   - Converting storage requires valid conversion descriptors with differing formats (`from != to`).
   - Column manifest references require generation > 0, base_version > 0, valid relative paths (`colstore/tablet-<tablet_id>/MANIFEST`), and cannot be present during the `SnapshotPinned` conversion phase. Columnar storage manifests use the `HTAPTBM1` envelope format (`crates/htap-convert/src/lib.rs:31-32`), which is distinct from tablet snapshot clone packages that use the `HTAPMNF1` movement manifest format (`crates/htap-movement/src/tablet.rs:50-51`).

### Test Coverage

The catalog model and validation rules are proven in `crates/htap-catalog/tests/catalog_recovery.rs`:
- `test_partitioning_legacy_decode_and_reopen`: Verifies backward compatibility when deserializing legacy catalog snapshots lacking partition fields.
- `test_range_partitioning_routing_and_boundaries`: Proves half-open `[lower, upper)` routing, edge-boundary inclusion/exclusion, and out-of-bounds rejection.
- `test_list_partitioning_routing`: Proves exact list routing and unmatched value rejection.
- `test_partitioning_duplicate_violations`: Validates rejection of duplicate partition names, overlapping range bounds, and duplicate list values.
- `test_partitioning_type_and_null_violations`: Validates rejection of nullable partition keys, null values, and type mismatches between bounds and schema.
- `test_partitioning_ownership_and_method_consistency`: Validates rejection of orphan partitions, mismatched parent IDs, and cross-method configurations.
- `test_partitioning_cas_and_reopen_lifecycle`: Proves atomic CAS updates and crash recovery of partitioned catalog snapshots.

---

## 3. Partition Routing and Local Topology Invariants

### Exact Routing Semantics (`TableDescriptor::route_partition_value`)

Partition routing determines which partition owns a row given its partition key value:

1. **Unpartitioned Tables:**
   - If `partitioning` is `None`, the table must have exactly one partition (`partitions.len() == 1`).
   - Returns `partitions[0]`.
2. **Range Partitioning:**
   - The value is evaluated against partitions in the order of `table.partitions`.
   - A partition matches if `range.lower <= *value && *value < range.upper`.
   - Returns the first matching `PartitionId`.
3. **List Partitioning:**
   - The value is evaluated against partitions in the order of `table.partitions`.
   - A partition matches if `part.list_values.iter().any(|v| v == value)`.
   - Returns the first matching `PartitionId`.
4. **Error Conditions:**
   - Null value: Partition keys cannot be null; returns `HtapError::InvalidArgument("partition key value cannot be null...")`.
   - Type mismatch: Value type must match key column type; returns `HtapError::InvalidArgument("partition key value type mismatch...")`.
   - Unmatched value: If no partition matches, returns `HtapError::InvalidArgument("partition key value <val> does not match any partition in table '<name>'")`.

### Monotonic ID and Generation Allocation

When creating a partitioned table in `LocalServer::create_partitioned_table`:
- Acquires `self.execution_lock`.
- Loads current catalog snapshot; computes `next_generation = catalog.generation + 1`.
- Table ID, partition IDs, tablet IDs, and replica IDs are allocated monotonically using checked addition:
  ```rust
  let next_id = max_existing_id.checked_add(1).ok_or(HtapError::CounterOverflow { counter: "..." })?;
  ```
- Submits the new `CatalogSnapshot` atomically via `self.catalog.compare_and_set(current_gen, next_snapshot)`.

### LocalServer 1:1:1 Local Topology Invariant and Validation

To ensure deterministic single-node execution, `LocalServer` maintains a 1:1:1 local topology invariant, distinguishing creation-time initialization defaults from runtime validation:

```text
1 Partition  <===>  1 Tablet (init bucket = 0)  <===>  1 Replica (init NodeId(1), Leader, Healthy)
```

1. **Creation Initialization (`LocalServer::create_partitioned_table`, `crates/htap-server/src/lib.rs:432-447`):**
   When a partitioned table is created, each partition is explicitly initialized with:
   - Exactly one tablet initialized with `bucket = 0` (`TabletDescriptor::new(tablet_id, partition_id, 0, vec![replica_id], next_generation)` at line 444).
   - Exactly one replica assigned to node 1 (`NodeId::new(1)`) marked as healthy leader (`ReplicaDescriptor::new(replica_id, tablet_id, NodeId::new(1), true, true, next_generation)` at lines 432-439).
   (The same `bucket = 0` and `NodeId(1)` defaults are used for the default `"p0"` partition in `LocalServer::execute_create_table`, `crates/htap-server/src/lib.rs:735-750`.)

2. **Runtime Topology Validation (`LocalServer::validate_partition`, `crates/htap-server/src/lib.rs:511-579`):**
   Before executing DML mutations, point lookups, analytical scans, or conversion steps, `LocalServer::validate_partition` verifies the structural integrity of every referenced partition:
   - **Partition Existence & Backlink:** The partition exists in the catalog (`lines 517-522`) and matches the table backlink (`partition.table_id == table_desc.id`, `lines 524-529`).
   - **One Tablet:** The partition contains exactly one tablet (`partition.tablets.len() == 1`, `lines 531-536`; returns `HtapError::Unsupported` otherwise).
   - **Tablet Existence & Backlink:** The tablet exists in the catalog (`lines 538-543`) and matches the partition backlink (`tablet.partition_id == partition.id`, `lines 545-550`).
   - **One Replica:** The tablet contains exactly one replica (`tablet.replicas.len() == 1`, `lines 552-557`; returns `HtapError::Unsupported` otherwise).
   - **Replica Existence & Backlink:** The replica exists in the catalog (`lines 559-564`) and matches the tablet backlink (`replica.tablet_id == tablet.id`, `lines 566-571`).
   - **Healthy Leader:** The replica is a healthy leader (`!replica.healthy || !replica.is_leader`, `lines 573-577`; returns `HtapError::Internal` otherwise).
   - **No Bucket or NodeId Checks:** `validate_partition` does **not** validate that `tablet.bucket == 0` or that `replica.node_id == NodeId(1)`. Those settings are construction-time initialization defaults established during table creation, not dynamic assertions checked during runtime partition validation.

### Test Coverage

- `test_partitioned_native_range_topology_catalog_reopen_continuation`: Proves range creation, 1:1:1 topology setup, catalog reopen, and subsequent ID continuation.
- `test_partitioned_native_list_topology_catalog_reopen_continuation`: Proves list creation, reopen, and continuation.
- `test_partitioned_boundary_unmatched_null_type_errors`: Proves runtime rejection of out-of-range keys, boundary conditions, null keys, and mismatched types.

---

## 4. Transactional DML and Point Read Routing

### Execution Lifecycle

All queries entering `LocalServer` follow a synchronized lifecycle:
1. Acquire `self.execution_lock.lock()`.
2. Load current catalog snapshot (`self.catalog.load()`).
3. Bind SQL statement against the catalog snapshot (`htap_sql::bind`).
4. Route statement based on its classified execution path (`classify_route`).

### Multi-Row `INSERT`: Single-Transaction Multi-Partition Routing

When inserting rows into a partitioned table (`LocalServer::execute_insert`):
1. **Per-Row Routing:** For each row in `insert.rows`, the partition key value is extracted from `row[partitioning.key_column]`.
2. **Partition Resolution:** Evaluates `table_desc.route_partition_value(catalog, val)`.
3. **Partition Validation:** Calls `self.validate_partition` to verify the local 1:1:1 topology and format route.
4. **Key Encoding:** Extracts primary key columns and encodes the LSM key (`encode_key`).
5. **Mutation Batching:** Creates `Mutation::Put { partition_id, key, row }`.
6. **Single Atomic Commit:**
   - All mutations spanning different partitions are encoded into a single payload: `RowstoreParticipant::encode_payload(&mutations)`.
   - A single `TransactionRequest` is committed through the 2PC manager (`self.txn_manager.commit_request(req)`).
   - **One Commit Version:** All rows across all touched partitions are committed atomically under a single transaction version step. No multi-version split or partial commits occur.

### Complete-PK `DELETE` and `SELECT`: Fast Path Preservation

When a statement specifies a complete primary key:
1. **Partition Key Position Resolution:** The partition key position within the primary key tuple is resolved dynamically:
   ```rust
   let pk_pos = table_desc.primary_key.iter()
       .position(|&col_idx| col_idx == partitioning.key_column)
       .ok_or_else(...)?;
   let part_val = pk_key.get(pk_pos).ok_or_else(...)?;
   let partition_id = table_desc.route_partition_value(catalog, part_val)?;
   ```
2. **Composite Primary Keys:** The partition key is not required to be the first column in a composite primary key. As long as it is part of the PK, its position within `pk_key` is correctly located (verified in `test_partitioned_composite_pk_partition_key_not_first`).
3. **Point `DELETE`:** Routes to `partition_id` and commits a single `Mutation::Delete` via `txn_manager`.
4. **Point `SELECT` (`Route::RowstorePointRead`):**
   - Obtains snapshot at `visible_version`.
   - Calls `self.engine.get(partition_id.as_u64(), &key, snapshot)` directly on the LSM rowstore.
   - **Rowstore Fast Path:** Completely bypasses analytical query planning, column scans, and conversion checks. Returns directly from the rowstore.

### Test Coverage

- `test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete`: Proves multi-row INSERT across 3 partitions in 1 version, followed by point reads and point deletes.
- `test_partitioned_composite_pk_partition_key_not_first`: Proves correct routing when the partition key is the second column in a composite PK `(tenant_id, id)`.

---

## 5. Analytical Execution: Pruning, Parallel Scans, and Global Merge

Analytical queries (`Route::OlapScan`) scan tables across multiple partitions while maintaining transactional snapshot consistency.

```text
AnalyticSelect (WHERE filter)
         │
         ▼
olap::prune_partitions (Snapshot-Frozen Catalog)
         │  Conservative pruning against partition key bounds
         ▼
Selected Partitions: [p0, p2] (preserves catalog sequence order)
         │
         ▼
Bounded Concurrent Workers (std::thread::scope, max scan_workers)
  Worker 0: scan_partition_compact(p0) -> (0, rows_p0)
  Worker 1: scan_partition_compact(p2) -> (1, rows_p2)
         │
         ▼
Deterministic Partition-Order Merge
  Sort results by partition index -> Concat rows in catalog order
         │
         ▼
Global OLAP Evaluator (olap::execute_analytic_select_compact)
  ├── Remap compact column indices
  ├── Residual filter evaluation (non-key or complex predicates)
  ├── Global aggregations (COUNT, SUM, MIN, MAX)
  ├── Grouped aggregation (GROUP BY with SQL NULL grouping)
  └── Global ORDER BY (ASC/DESC, NULLS FIRST/LAST, tie-breaking)
```

### Snapshot Freeze

At the start of `LocalServer::execute_analytic_select`:
- Catalog state is pinned from the current snapshot.
- MVCC snapshot is frozen: `snapshot = Snapshot::new(self.txn_manager.visible_version())`.
- All partition scans observe this exact snapshot version, guaranteeing point-in-time cross-partition consistency.

### Conservative Partition-Key Pruning (`htap-server::olap::prune_partitions`)

Pruning inspects `AnalyticFilter` leaves targeting `partitioning.key_column`. Pruning is strictly conservative: partitions are pruned only when provably impossible to match.

1. **Null and Comparison with Null:**
   - `IsNull`: Because partition keys are catalog-enforced non-null, `key IS NULL` prunes **all** partitions (returns empty).
   - `IsNotNull`: Retains all partitions.
   - Comparison with `NULL` (e.g. `key = NULL`): SQL 3-valued logic yields UNKNOWN/false; prunes **all** partitions.
2. **Type Safety:** Comparisons with mismatched data types retain all partitions conservatively.
3. **Range Partition Pruning:**
   - `Eq`: Retains partition if `lower <= value && value < upper`; prunes otherwise.
   - `Lt`: Retains partition if `lower < value`; prunes if `lower >= value`.
   - `Lte`: Retains partition if `lower <= value`; prunes if `lower > value`.
   - `Gt`: Retains partition if `upper > value`; prunes if `upper <= value`.
   - `Gte`: Retains partition if `upper > value`; prunes if `upper <= value`.
   - `NotEq`: Retains all partitions conservatively.
4. **List Partition Pruning:**
   - `Eq`: Retains partition if `list_values.contains(value)`; prunes otherwise.
   - `Lt`: Retains partition if any value in `list_values < value`.
   - `Lte`: Retains partition if any value in `list_values <= value`.
   - `Gt`: Retains partition if any value in `list_values > value`.
   - `Gte`: Retains partition if any value in `list_values >= value`.
   - `NotEq`: Retains all partitions conservatively.
5. **Conjunctions and Residual Non-Key Filters:** Non-partition-key leaves alone do not prune (evaluating conservatively to retaining all partitions), but partition-key leaves within an `AND` conjunction can still prune conservatively alongside residual non-key filters. Any non-key predicates or unsupported expressions are preserved as residual filters and evaluated post-scan (`evaluate_filter` during global OLAP evaluation).
6. **Order Preservation:** Retained partitions strictly maintain their original catalog order.

### Bounded In-Process Parallelism and Deterministic Merge

1. **Worker Count:**
   - Bounded by `self.scan_workers` (default `DEFAULT_SCAN_WORKERS = 4`):
     ```rust
     let num_workers = self.scan_workers.max(1).min(n_parts.max(1));
     ```
   - If `n_parts <= 1` or `num_workers <= 1`, execution runs sequentially on the calling thread.
2. **Scoped Thread Execution:**
   - Uses `std::thread::scope` to spawn worker threads without heap allocation of thread handles.
   - Partitions are distributed round-robin: `idx % num_workers == worker_id`.
   - Each worker produces indexed results `(idx, Result<Vec<Row>>)`.
3. **Deterministic Partition-Order Merge:**
   - Worker results are sorted by partition index (`indexed_results.sort_by_key(|(idx, _)| *idx)`).
   - Rows are concatenated in partition order into `all_compact_rows`.
   - Verified across varying worker counts (`1, 2, 4, 8`) in `test_scan_worker_count_equivalence`.

### Global Post-Scan OLAP Evaluation (`execute_analytic_select_compact`)

After rows are merged across partitions:
- **Projection Remapping:** Compact row columns (union of primary key and requested columns) are mapped to projected positions.
- **Residual Filters:** Evaluates complex or non-pushdown filters (`evaluate_filter`).
- **Global Aggregations:** Computes `COUNT(*)`, `COUNT(col)`, `SUM(int/float)`, `MIN`, and `MAX` across the combined dataset.
- **Grouped Aggregation:** Evaluates `GROUP BY` with deterministic key sorting and SQL NULL grouping.
- **Global `ORDER BY`:** Sorts merged rows by unqualified columns with ASC/DESC, NULLS FIRST/LAST, and deterministic tie-breaking.

### Test Coverage

- `test_partition_pruning_range_and_list_and_conservative_cases`: Validates pruning for Eq, Lt, Lte, Gt, Gte, IsNull, NotEq, and range/list boundaries.
- `test_scan_worker_count_equivalence`: Validates identical output across worker counts 1, 2, 4, and 8.
- `test_multi_partition_global_aggregates_and_groups`: Proves cross-partition COUNT, SUM, MIN, MAX, and GROUP BY.
- `test_multi_partition_order_by_directions_nulls_and_tie_breaking`: Proves cross-partition ORDER BY with NULL handling.
- `test_partitioned_olap_across_partitions_and_empty_aggregate`: Validates cross-partition empty table aggregates and post-insert results.

---

## 6. Storage Behavior and Conversion Boundary

### Per-Partition Storage Handling (`scan_partition_compact`)

Each partition is scanned according to its `StorageDescriptor`:

```text
StorageDescriptor
├── Row
│     └── engine.scan_partition(partition_id, snapshot)
│           └── collapse_entries_to_rows(&entries)
│
├── Column
│     ├── Validate catalog manifest against colstore/tablet-<tablet_id>/MANIFEST
│     └── read_column_partition_compact_core
│           ├── Scan base columnar segments (pushdown predicate leaf)
│           ├── Scan rowstore delta entries
│           └── Overlay deltas, suppress deleted/updated base rows
│
└── Converting
      ├── If column manifest present: read_column_partition_compact_core
      └── If no manifest and phase == SnapshotPinned: rowstore scan fallback
```

1. **`StorageDescriptor::Row`:**
   - Scans partition entries from rowstore memtable and SSTs (`engine.scan_partition`).
   - Collapses versions into visible rows (`htap_convert::collapse_entries_to_rows`).
   - Extracts compact requested columns.
2. **`StorageDescriptor::Column`:**
   - Verifies tablet has a valid `column_manifest` in catalog.
   - Opens `<root>/colstore/tablet-<tablet_id>/MANIFEST` (in `HTAPTBM1` envelope format) and validates that disk generation matches catalog generation.
   - Executes `htap_convert::read_column_partition_compact_core`:
     - Reads columnar base segments with single-leaf predicate pushdown into `SegmentReader::scan`.
     - Scans rowstore for post-base mutations (`engine.scan_partition`).
     - Suppresses stale base rows and overlays delta mutations in deterministic PK order.
3. **`StorageDescriptor::Converting`:**
   - If manifest is published: Reads columnar base plus rowstore deltas via `read_column_partition_compact_core`.
   - If manifest is absent and phase is `ConversionPhase::SnapshotPinned`: Safely falls back to full rowstore scan (`engine.scan_partition`).
   - If manifest is absent in any later phase: Returns `HtapError::InvalidArgument`.

### Single-Partition Format Conversion Guard

Row-to-column format conversion is performed partition-by-partition. However, the table-level conversion API `LocalServer::convert_table` enforces a single-partition guard:
```rust
pub fn convert_table(&self, table_name: &str) -> Result<TabletColumnManifest> {
    let _guard = self.execution_lock.lock();
    let catalog = self.catalog.load()?.unwrap_or_else(CatalogSnapshot::empty);
    let (_, partition) = self.resolve_single_partition_table(table_name, &catalog)?;
    ...
}
```

If `convert_table` is called on a multi-partition table, `resolve_single_partition_table` detects `partitions.len() != 1` and rejects the operation with `HtapError::Unsupported("table '<name>' must have exactly one partition, found <n>")`. Coordinated conversion across multi-partition tables is explicitly deferred.

### Test Coverage

- `test_partition_storage_format_row_column_converting_equivalence`: Proves query equivalence across partitions with mixed storage formats (`Row`, `Column`, `Converting`).
- `test_convert_table_multi_partition_guard`: Verifies rejection of `convert_table` on multi-partition tables.

---

## 7. Reopen, Recovery, and Persistence

Partition state is persistent and crash-safe across server restarts, qualified as tested process-crash, abrupt termination (`SIGKILL`), reopen recovery, and atomic publication behavior (e.g. atomic staging `.tmp` -> fsync -> rename and catalog CAS updates). As documented in [`docs/LIMITATIONS.md`](LIMITATIONS.md), process-crash testing proves replay integrity across abrupt process death but does not prove fsync durability or power-loss resilience, as the OS page cache survives process death; there is currently **no power-loss or fsync durability proof** (machine-level power-loss and storage fault injection such as `dm-flakey` are deferred from the local MVP).

### Server Reopen Lifecycle (`LocalServer::open`)

When `LocalServer::open(root)` is called:
1. **OS Process Lock:** Acquires non-blocking exclusive advisory lock on `<root>/LOCK` (`ProcessLock`).
2. **Catalog Recovery:** Opens `LocalCatalogStore` at `<root>/catalog`. Reads durable snapshot from `<root>/catalog/CATALOG`, verifying integrity and deserializing partition metadata. Calls `CatalogSnapshot::validate()` to ensure no corrupted or orphaned entities exist.
3. **Rowstore Recovery:** Opens `htap_rowstore::Engine` at `<root>/rowstore`. Replays WAL segments (`wal/*.wal`), mounts immutable SSTs (`sst/*.sst`), and reads visible version watermark.
4. **Transaction Journal Recovery:** Opens `TransactionManager` at `<root>/txn.journal`. Re-registers `RowstoreParticipant` (wrapping `Engine`), recovers 2PC transaction states, and republishes committed versions.
5. **Columnar Storage Root:** Ensures `<root>/colstore/` directory exists for tablet columnar manifests (`<root>/colstore/tablet-<tablet_id>/MANIFEST`) and segment files.

### Monotonic ID Continuation

Upon recovery, `LocalServer` calculates current maximum IDs across all existing tables, partitions, tablets, and replicas in the catalog:
```rust
let max_partition_id = catalog.partitions.iter().map(|p| p.id.as_u64()).max().unwrap_or(0);
```
Subsequent table or partition creation operations increment from `max_partition_id + 1`, guaranteeing that partition IDs never collide or regress across crash/reopen cycles (`test_partitioned_native_range_topology_catalog_reopen_continuation`).

For detailed operational procedures, disk layouts, and disaster recovery commands, see [`docs/OPERATIONS.md`](OPERATIONS.md).

---

## 8. Architectural Boundaries and Deferred Scope

To maintain rigorous production invariants, HTAP explicitly delineates implemented capabilities from deferred distributed architecture:

| Capability / Area | Status in Local Server | Architectural / Deferred Status |
|---|---|---|
| **SQL Partition DDL** | `CREATE TABLE` creates unpartitioned `p0`; MySQL partition DDL rejected (`InvalidArgument`) | Deferred until SQL parser/binder upgrade supports lossless MySQL partition syntax |
| **Partition Lifecycle DDL** | None | `ALTER TABLE ADD/DROP/REORGANIZE PARTITION` deferred |
| **Topology Specification** | Finite `PartitionTopology::Range` and `List` via `LocalServer::create_partitioned_table` | Catch-all `MAXVALUE` and `DEFAULT` options deferred |
| **Tablet Sharding / Hashing** | Exactly 1 bucket-0 tablet per partition | Hash bucket rings, sub-partitioning, and dynamic tablet splitting deferred |
| **Replica Topology** | Exactly 1 local leader replica on `NodeId(1)` | Multi-node replica placement, Raft consensus groups, and failover deferred |
| **Multi-Partition DML** | Multi-row INSERT committed in 1 transaction payload and version step | Cross-partition row movement on partition key UPDATE deferred |
| **Point Reads / Deletes** | Partition-routed point read (`Engine::get`) and delete | Distributed point lookup RPC fanout deferred |
| **OLAP Execution** | Conservative range/list pruning; bounded local workers; deterministic merge | Distributed scan fanout across remote nodes deferred |
| **OLAP Resource Controls** | Bounded in-process worker pool (`scan_workers`) | Disk spilling, query cancellation tokens, and CPU/memory quotas deferred |
| **Storage Conversion** | Partition-scoped row-to-column conversion; `convert_table` guarded to single-partition | Coordinated multi-partition conversion and reverse Column-to-Row conversion deferred |

### Catalog Metadata vs. Physical Serving

A fundamental architectural distinction in HTAP is the difference between catalog representation and local physical serving:
- **Catalog Model (`htap-catalog`):** Generalized and forward-compatible. Represents arbitrary partitions, multiple tablet buckets per partition, and multiple replica nodes per tablet with health and leadership flags.
- **Physical Serving (`htap-server`):** Constrained to single-node local invariants. Specifically, creation logic (`create_partitioned_table`, lines 432–447) initializes each partition with exactly one bucket-0 tablet and one `NodeId(1)` leader replica. At runtime, `LocalServer::validate_partition` (lines 511–579) enforces that each partition has exactly one tablet and one healthy leader replica with valid bidirectional backlinks, but does not validate `bucket == 0` or `node_id == NodeId(1)`.

---

## 9. Architecture and Execution Flowchart

The following flowchart illustrates the lifecycle of partitioned tables, from native administrative creation to transactional DML routing and analytical evaluation:

```mermaid
flowchart TD
    AdminApi["Admin API<br/>LocalServer::create_partitioned_table"] --> ValidDef["Validate PartitionedTableDefinition<br/>(non-empty topology, schema check)"]
    ValidDef --> CatCas["Catalog CAS Commit<br/>(monotonic IDs & generation increment)"]
    CatCas --> TopoInv["Enforce 1:1:1 Local Topology<br/>1 Tablet (init bucket 0) + 1 Replica (init Node1 Leader)"]
    
    TopoInv --> StmtFork{"Incoming SQL Statement Route"}
    
    StmtFork -->|"Multi-Row INSERT"| InsertPath["Extract partition key per row<br/>Route via TableDescriptor::route_partition_value<br/>Single atomic TransactionRequest & version"]
    StmtFork -->|"Complete-PK DELETE"| DeletePath["Find partition key at PK position<br/>Route to partition<br/>Rowstore delete mutation"]
    StmtFork -->|"Complete-PK SELECT"| SelectPath["Find partition key at PK position<br/>Route to partition<br/>Preserve Engine::get fast path"]
    StmtFork -->|"Analytic SELECT"| OlapPath["Freeze visible version snapshot<br/>olap::prune_partitions (Range / List)"]
    
    OlapPath --> ScanWorkers["Bounded Scan Workers (scan_workers)<br/>Concurrent scan_partition_compact<br/>(Row / Column / Converting)"]
    ScanWorkers --> MergeRows["Deterministic Partition-Order Merge<br/>Sort by partition sequence index"]
    MergeRows --> Evaluator["Global OLAP Evaluator<br/>Residual filter -> Aggregates -> GROUP BY -> ORDER BY"]
```
