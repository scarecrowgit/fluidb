# Attribution

This project is an **independent Rust implementation**. It contains no code
copied from StarRocks or from Apache ZooKeeper. The designs listed below were
studied as architectural references and re-derived; no source file from any
referenced project was vendored, copied, or machine-translated.

---

## StarRocks

StarRocks was studied as an architectural reference (branch `main`, commit
`444cb3cf593`). See [`docs/RESEARCH.md`](./docs/RESEARCH.md) for the full
record of what was adopted and what was rejected.

> StarRocks
> Copyright 2021-present StarRocks, Inc.
>
> Licensed under the Apache License, Version 2.0 (the "License");
> you may not use this file except in compliance with the License.
> You may obtain a copy of the License at
>
>     http://www.apache.org/licenses/LICENSE-2.0
>
> Unless required by applicable law or agreed to in writing, software
> distributed under the License is distributed on an "AS IS" BASIS,
> WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
> See the License for the specific language governing permissions and
> limitations under the License.

### Design elements re-derived from StarRocks

| Design element | What it is | Crate |
| -------------- | ---------- | ----- |
| Delete-vector merge-on-read model | Per-segment, per-version Roaring bitmap of dead rowids; reads are a UNION with one bitmap ANDNOT instead of a key-ordered merge. | `htap-rowstore`, `htap-colstore` (planned) |
| `(rssid << 32) \| rowid` primary-index packing | 64-bit packing of a tablet-global segment id and an in-segment row ordinal as the primary index value. | `htap-rowstore` (planned) |
| Inverted-version key ordering for delete-vector lookup | Storing `i64::MAX - version` in the key so versions sort descending and a snapshot read is one forward scan. | `htap-rowstore` (planned) |
| Segment footer + 12-byte trailer with CRC32C and magic | Append-only segment layout terminated by `footer_length \| crc32c \| magic`, with a speculative tail read on open. | `htap-colstore` (planned) |
| Per-page zone maps with four-state null encoding | `min`/`max`/`has_null`/`has_not_null` per data page, converted to row-id ranges via the ordinal index. | `htap-colstore` (planned) |
| Adaptive page compression on a space-saving threshold | Compress only if saving exceeds a threshold; infer compression from stored size vs. recorded uncompressed size, with no explicit flag. | `htap-colstore` (planned) |
| Two-phase commit/publish visibility with version-density gating | Commit assigns a version and journals one record; a separate idempotent publish makes it visible once the version is exactly `visibleVersion + 1`. | `htap-txn` (planned) |
| Layered epoch + lease fencing | Term fence (compare-and-set epoch claim) and session fence (epoch-keyed lease with a generation counter) exposed through the `Coordinator` trait. | `htap-coord` (planned) |

---

## Apache Kudu

The order-preserving composite key encoding described in
[`docs/RESEARCH.md`](./docs/RESEARCH.md) (sign-bit-flipped big-endian
integers; `0x00` escaped as `0x00 0x01` with a `0x00 0x00` terminator for
non-final string key components; sentinel markers for partial-key range
bounds) traces to **Apache Kudu**, via StarRocks, which credits Kudu for it.

> Apache Kudu
> Copyright The Apache Software Foundation
>
> Licensed under the Apache License, Version 2.0 (the "License");
> you may not use this file except in compliance with the License.
> You may obtain a copy of the License at
>
>     http://www.apache.org/licenses/LICENSE-2.0
>
> Unless required by applicable law or agreed to in writing, software
> distributed under the License is distributed on an "AS IS" BASIS,
> WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
> See the License for the specific language governing permissions and
> limitations under the License.

---

## Apache ZooKeeper

Apache ZooKeeper (Apache-2.0) is an architectural **protocol reference**
for future distributed coordination proposals (reference only / not implemented;
see [`docs/LIMITATIONS.md`](./docs/LIMITATIONS.md) and ADR-006 in
[`docs/DECISIONS.md`](./docs/DECISIONS.md)). Neither a ZooKeeper backend nor a
`zookeeper-async` client crate dependency is implemented in this codebase;
all coordination is implemented via `htap-coord::LocalCoordinator`.

> Apache ZooKeeper
> Copyright The Apache Software Foundation
>
> Licensed under the Apache License, Version 2.0 (the "License");
> you may not use this file except in compliance with the License.
> You may obtain a copy of the License at
>
>     http://www.apache.org/licenses/LICENSE-2.0
>
> Unless required by applicable law or agreed to in writing, software
> distributed under the License is distributed on an "AS IS" BASIS,
> WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
> See the License for the specific language governing permissions and
> limitations under the License.

---

## Apache DataFusion sqlparser-rs

The workspace vendors a minimally patched copy of `sqlparser` 0.62.0 under `vendor/sqlparser`
licensed under Apache-2.0.

- **Upstream Project:** Apache DataFusion `sqlparser-rs`
- **Upstream Repository:** `https://github.com/apache/datafusion-sqlparser-rs`
- **Release / Source Artifact:** Tag `0.62.0` (crates.io crate `sqlparser-0.62.0.crate`)
- **License:** Apache License, Version 2.0 (preserved in `vendor/sqlparser/LICENSE.TXT`)
- **Local Patch Scope and Files:**
  - `vendor/sqlparser/src/ast/ddl.rs`: Adds AST types `MysqlPartitionBy`, `MysqlPartitionDef`, `MysqlPartitionValues`, and `MysqlLessThanBound`; adds `pub mysql_partition_by: Option<MysqlPartitionBy>` to `CreateTable`.
  - `vendor/sqlparser/src/ast/mod.rs`: Re-exports `MysqlPartitionBy`, `MysqlPartitionDef`, `MysqlPartitionValues`, `MysqlLessThanBound`.
  - `vendor/sqlparser/src/ast/helpers/stmt_create_table.rs`: Adds `pub mysql_partition_by: Option<MysqlPartitionBy>` and builder method `mysql_partition_by` to `CreateTableBuilder`.
  - `vendor/sqlparser/src/parser/mod.rs`: Implements `maybe_parse_mysql_partition_by` and `parse_mysql_partition_def` to parse MySQL `PARTITION BY RANGE [COLUMNS] (...)` and `PARTITION BY LIST [COLUMNS] (...)` with `VALUES LESS THAN (...)` / `MAXVALUE` and `VALUES IN (...)`, invoked during table creation parsing.
- **How to Refresh / Rebase:**
  1. Obtain target upstream release or commit from `https://github.com/apache/datafusion-sqlparser-rs`.
  2. Extract files into `vendor/sqlparser`, ensuring no nested `.git` metadata is preserved.
  3. Re-apply the MySQL partition AST definitions in `src/ast/ddl.rs`, builder integration in `src/ast/helpers/stmt_create_table.rs`, module exports in `src/ast/mod.rs`, and parser hooks in `src/parser/mod.rs`.
  4. Ensure `vendor/sqlparser/LICENSE.TXT` and `vendor/sqlparser/Cargo.toml` are intact.
  5. Run `cargo check -p sqlparser` and workspace tests (`cargo test --workspace`) to verify compatibility.

> sqlparser-rs
> Copyright 2018-present Apache DataFusion Authors
>
> Licensed under the Apache License, Version 2.0 (the "License");
> you may not use this file except in compliance with the License.
> You may obtain a copy of the License at
>
>     http://www.apache.org/licenses/LICENSE-2.0
>
> Unless required by applicable law or agreed to in writing, software
> distributed under the License is distributed on an "AS IS" BASIS,
> WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
> See the License for the specific language governing permissions and
> limitations under the License.

---

## Statement

No StarRocks, Apache Kudu, or Apache ZooKeeper source file was vendored,
copied, or machine-translated into this repository. All listed design elements
were studied as architectural references; only the local MVP subset is
implemented independently in Rust, while proposals such as delete vectors,
shared multi-format WAL, openraft, DataFusion, and ZooKeeper coordination
backends remain reference proposals or deferred future work.
The `sqlparser` crate is vendored under `vendor/sqlparser` under Apache-2.0 with provenance recorded above.
