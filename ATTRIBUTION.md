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
| MySQL client/server protocol mechanisms | Handshake/capability negotiation state machine, native-password scramble, column type mapping, and packet framing as implemented by the StarRocks FE (`fe/fe-core/src/main/java/com/starrocks/mysql/`). Used as a mechanism reference only; `htap-wire` was written from the public protocol description and verified against a real driver (see `docs/RESEARCH.md` finding 13, ADR-016). | `htap-wire` |

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
  - `vendor/sqlparser/src/keywords.rs`: Adds `REORGANIZE` keyword.
  - `vendor/sqlparser/src/ast/ddl.rs`: Adds AST types `MysqlPartitionBy`, `MysqlPartitionDef`, `MysqlPartitionValues`, and `MysqlLessThanBound`; adds `pub mysql_partition_by: Option<MysqlPartitionBy>` to `CreateTable`; adds `AddPartition`, `DropPartition`, and `ReorganizePartition` operations to `AlterTableOperation` with `Display` implementations.
  - `vendor/sqlparser/src/ast/spans.rs`: Implements `Spanned` for `AddPartition`, `DropPartition`, and `ReorganizePartition`.
  - `vendor/sqlparser/src/ast/mod.rs`: Re-exports `MysqlPartitionBy`, `MysqlPartitionDef`, `MysqlPartitionValues`, `MysqlLessThanBound`.
  - `vendor/sqlparser/src/ast/helpers/stmt_create_table.rs`: Adds `pub mysql_partition_by: Option<MysqlPartitionBy>` and builder method `mysql_partition_by` to `CreateTableBuilder`.
  - `vendor/sqlparser/src/parser/mod.rs`: Implements `maybe_parse_mysql_partition_by` and `parse_mysql_partition_def` to parse MySQL `PARTITION BY RANGE [COLUMNS] (...)` and `PARTITION BY LIST [COLUMNS] (...)` with `VALUES LESS THAN (...)` / `MAXVALUE` and `VALUES IN (...)`, invoked during table creation parsing; adds support in `parse_alter_table_operation` for MySQL `ALTER TABLE ... ADD PARTITION (...)`, `DROP PARTITION ...`, and `REORGANIZE PARTITION ... INTO (...)`.
  - `vendor/sqlparser/src/parser/mod.rs` (Phase 19, commit `dfd0240`): in the `SET ... TRANSACTION` branch of the `SET` parser, `SET LOCAL TRANSACTION` and `SET SESSION TRANSACTION` now set `Set::SetTransaction.session = true` (previously always `false` for the MySQL forms, which silently dropped the `SESSION` keyword), and `SET GLOBAL TRANSACTION` is rejected with a parse error.
- **How to Refresh / Rebase:**
  1. Obtain target upstream release or commit from `https://github.com/apache/datafusion-sqlparser-rs`.
  2. Extract files into `vendor/sqlparser`, ensuring no nested `.git` metadata is preserved.
  3. Re-apply keyword definition in `src/keywords.rs`, MySQL partition AST definitions and ALTER partition operations in `src/ast/ddl.rs`, span implementations in `src/ast/spans.rs`, builder integration in `src/ast/helpers/stmt_create_table.rs`, module exports in `src/ast/mod.rs`, and parser hooks in `src/parser/mod.rs` (including the `SET ... TRANSACTION` session-scope change above).
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

## Transaction Processing Performance Council (TPC-H)

`crates/htap-tpch` reproduces two pieces of the TPC Benchmark(TM) H Standard Specification (Revision 3.0.1, 28
April 2022) verbatim, under the specification's own copying-by-permission notice: all 22 published query texts
(`queries::query`, shipped in Phase 17 Batch B checkpoint 1) and, as of Phase 17 task B8, Appendix A's 41-row
query-order permutation table (`drivers::QUERY_ORDER`, used by `drivers::query_order`). Both carry the notice
below in their module documentation. The specification itself attributes Appendix A's origin to Moses &
Oakford; that attribution is carried alongside the TPC notice in `crates/htap-tpch/src/drivers.rs`'s module doc.
No TPC-H Tools distribution source (QGen, DBGen, or the reference implementation) was vendored, copied, or
machine-translated; `crates/htap-tpch::generate` is an independent, from-specification row generator (see
`docs/LIMITATIONS.md`'s "TPC-H workload kit scope and deferred features"), and `crates/htap-tpch::drivers`
computes no official TPC-H metric (see ADR-030 in `docs/DECISIONS.md`). This project makes no TPC-H compliance
or comparability claim. See [`docs/TPCH-DISCLOSURE.md`](./docs/TPCH-DISCLOSURE.md) for the full TPC Policies
§8.1.5 disclaimer, the §8.3.2 deviations list, and the §8.3.3 non-comparability statement (task B9); this
section remains the canonical record of what is reproduced verbatim and why.

> TPC Benchmark(TM) H Standard Specification, Revision 3.0.1, 28 April 2022.
> Copyright 1993-2022 Transaction Processing Performance Council. Copying is
> by permission of the Transaction Processing Performance Council.

> Appendix A's query-order permutation table is derived by the TPC from:
> F. Moses and O. Oakford, *Tables of Random Permutations*, Stanford University Press, 1963, pp. 52-53.

---

## Transaction Processing Performance Council (TPC-C)

`crates/htap-tpcc` is an independent, from-specification workload kit derived from the TPC Benchmark(TM) C
Standard Specification (Revision 5.11, February 2010): the nine-table schema, the population rules of Clause
4.3.3.1, the five transaction profiles, the twelve consistency conditions of Clause 3.3.2, the isolation tests
of Clause 3.4.2 (adapted to optimistic concurrency control) and the transaction-mix rules of Clause 5.2. It
reproduces no text of the specification in the source tree: only specification-defined data values (the ten `C_LAST` syllables and
the two worked examples of Clause 4.3.2.3, and numeric constants such as the population cardinalities and the
23-card deck composition) appear in code, and clauses are cited by number. No TPC-C sample program (Appendix A of the
specification), TPC-provided software or other implementation's source was vendored, copied, or
machine-translated. The kit computes no official TPC-C metric (`tpmC`, price/performance, availability date; see
ADR-032 in `docs/DECISIONS.md`), is not audited, and this project makes no TPC-C compliance or comparability
claim. See [`docs/TPCC-DISCLOSURE.md`](./docs/TPCC-DISCLOSURE.md) for the TPC Policies §8.1.5 disclaimer, the
§8.3.2 deviations list and the §8.3.3 non-comparability statement; this section remains the canonical record of
what is drawn from the specification and under which notice. The specification's copying-by-permission notice
(page 4) is carried here for the title and date, since no specification text is reproduced in the source tree.
The publication cited is TPC Benchmark(TM) C Standard Specification, Revision 5.11, February 2010, Copyright 2010
Transaction Processing Performance Council. The notice reads:

> TPC Benchmark(TM), TPC-C, and tpmC are trademarks of the Transaction Processing Performance Council.
> Permission to copy without fee all or part of this material is granted provided that the TPC copyright notice,
> the title of the publication, and its date appear, and notice is given that copying is by permission of the
> Transaction Processing Performance Council. To copy otherwise requires specific permission.

The TPC Policies (v6.20, November 2024) govern derived work: §8.1.4 (the "Derived from" prefix; no Primary or
Optional Metric use), §8.1.5 (the disclaimer), §8.3.2 and §8.3.3.

---

## Published literature (Phase 19, SERIALIZABLE isolation)

The SERIALIZABLE level (ADR-033) was re-derived from published papers (Kung and Robinson 1981; Cahill, Röhm and Fekete 2008;
Fekete et al. 2004 and 2005; Ports and Grittner 2012; Adya 1999) and the MySQL 8.0 manual's `SET TRANSACTION` description;
see `docs/RESEARCH.md`. No source code from any project was used for it, and none of those works is implemented as published.

---

## Statement

No StarRocks, Apache Kudu, or Apache ZooKeeper source file was vendored,
copied, or machine-translated into this repository. All listed design elements
were studied as architectural references; only the local MVP subset is
implemented independently in Rust, while proposals such as delete vectors,
shared multi-format WAL, openraft, DataFusion, and ZooKeeper coordination
backends remain reference proposals or deferred future work.
The `sqlparser` crate is vendored under `vendor/sqlparser` under Apache-2.0 with provenance recorded above.
