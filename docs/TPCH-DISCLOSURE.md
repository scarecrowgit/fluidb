# Derived from TPC-H — Compliance and Deviations Disclosure

Phase 17, task B9. This document is the required TPC-H compliance/deviations disclosure for
`crates/htap-tpch`, the workload kit built across Phase 17 Batch A and Batch B (tasks B2, B3, B5b,
B5c, B6, B7, B8, plus the shared prerequisite work and task F6). It covers the TPC Policies
requirements that apply to work derived from a TPC Benchmark Standard (TPC Policies v6.20,
November 2024, §8.1.4, §8.1.5, §8.3.2, §8.3.3) and the TPC Benchmark™ H Standard Specification
Revision 3.0.1 (28 April 2022, "the specification"). Per §8.1.4, this document's title and every place
this project's own workload kit is named as "the TPC-H Benchmark" carry the "Derived from" prefix; a
bare "TPC-H" elsewhere names the specification itself, one of its clauses, or repository shorthand
already used throughout this project's documentation (e.g. "TPC-H schema", "TPC-H fixture", "TPC-H
workload kit"), consistent with existing usage in `docs/ARCHITECTURE.md`, `docs/LIMITATIONS.md`, and
`docs/PROGRESS.md`.

This document makes no legal claim on the TPC's behalf and is not itself a TPC submission; it exists
so a reader never needs to reconstruct from code and tests alone which published behavior this kit
reproduces, changes, or omits.

## 1. Required disclaimer and non-comparability statements (verbatim)

TPC Policies §8.1.5 requires the following disclaimer, verbatim, on all work derived from a TPC
Benchmark Standard:

> This workload is derived from the TPC-H Benchmark and is not comparable to published TPC-H
> Benchmark results, as this implementation does not comply with all requirements of the TPC-H
> Benchmark.

TPC Policies §8.3.2 requires:

> All deviations from the TPC Benchmark Standard in question must be explicitly noted.

Section 3 below is that explicit list.

TPC Policies §8.3.3 requires:

> Results based on a non-TPC Benchmark must be clearly identified as not being comparable to an
> official TPC Benchmark Result.

Applied here: no result produced by this crate — no query timing, no `power_test`/`throughput_test`
report, no loaded dataset — is a TPC Benchmark Result or comparable to one. Nothing in this crate has
been submitted for or has passed a TPC audit.

TPC Policies §8.1.4 requires the "Derived from" naming prefix before every instance of the benchmark
name in public material describing derived work, and separately prohibits using any TPC Benchmark
Standard's Primary or Optional Metric name in derived work:

> The use of any Primary Metric or Optional Metric of a TPC Benchmark Standard in a work that is
> derived from TPC material is not allowed.

Section 6 below states how this crate satisfies that prohibition.

## 2. What this kit is

`crates/htap-tpch` is a local diagnostic kit: an in-process schema, query-text, data-generation,
bulk-load, refresh-function, and correctness/diagnostic-driver library for exercising this engine
against a TPC-H-shaped workload. It is **not audited**, has gone through no TPC review process, and
produces **no TPC metric** — official or unofficially-relabeled. Nothing it computes may be quoted as
a `QphH`, `QppH`, `QthH`, or `$/QphH` figure, or presented as comparable to one (§1 above). See
`docs/LIMITATIONS.md`'s "TPC-H workload kit scope and deferred features" and `docs/ARCHITECTURE.md`'s
"TPC-H workload kit foundations" for the full build history; this document is the deviations/
compliance record those sections point to.

## 3. Deviations (TPC Policies §8.3.2)

Each item below names the specification clause it departs from, the code, and a runnable test.

1. **Query 15 uses a common table expression, not a view (Clause 2.4.15.2).** The clause defines
   query 15 as `create view` / `select` / `drop view`; `CREATE VIEW` is not implemented in this
   engine (see `docs/LIMITATIONS.md`'s general SQL-layer scope), so the kit substitutes a CTE that
   otherwise carries the specification's own column names — the CTE construct itself is the only
   remaining difference from the published text. Code: `crates/htap-tpch/src/queries.rs` (query 15
   text). Tests: `crates/htap-tpch/tests/correctness_fixture.rs::test_q15`;
   `crates/htap-server/tests/tpch_sql_prerequisites.rs::test_cte_referenced_twice_with_column_list`.

2. **Arbitrary and fractional scale factors (Clause 4.1.3.1).** The clause permits only
   1/10/30/100/300/1000/3000/10000/30000/100000 and explicitly forbids intermediate values; this
   kit's `scale_factor::scale_factor` accepts any positive fractional scale factor (floored to a
   minimum of one row per table). See ADR-027. Code: `crates/htap-tpch/src/scale_factor.rs`. Tests:
   `scale_factor::tests::{test_scale_factor_exact, test_scale_factor_fractional,
   test_scale_factor_rounding_boundary, test_scale_factor_floor, test_scale_factor_overflow,
   test_scale_factor_invalid_input, test_scale_factor_exact_vs_float_discrimination}`.

3. **Stream counts are chosen by the caller and not enforced against Table 11.** `throughput_test`'s
   `stream_count` is an explicit parameter; `table_11_stream_count(sf)` reproduces Table 11's
   informational per-scale-factor values but is never consulted or enforced by either driver. See
   ADR-030. Code: `crates/htap-tpch/src/drivers.rs::{throughput_test, table_11_stream_count}`. Tests:
   `drivers::tests::{test_table_11_stream_count_exact_values,
   test_throughput_rejects_invalid_inputs_before_workers}`.

4. **Fixed validation parameters for every stream, not Clause 2.1.3.3's random substitution.** Every
   query in every driver stream binds through `params::fixed_parameters` — the specification's own
   published validation defaults — rather than a per-stream random draw from each query's parameter
   domain; there is no QGen-equivalent parameter generator in this crate. See ADR-030. Code:
   `crates/htap-tpch/src/params.rs`, consumed by `queries::query` and both drivers. Test:
   `params::tests::test_fixed_parameters_appear_in_query_texts`.

5. **The generator is an original from-specification implementation, not `dbgen`.** Two separate
   deviations: **(a)** it uses a hand-rolled two-sub-stream splitmix64 PRNG rather than reproducing
   any other implementation's sequence, because the specification names no PRNG algorithm at all for
   row data (Clause 4.2.2.1 defines "random" only as uniform and independent; Clause 2.1.3.3's
   seeding discipline governs query *parameters*, not row generation); and **(b)** comment and
   address text use original vocabulary rather than Clause 4.2.2.13's comment-grammar word lists
   (nouns, verbs, adjectives, adverbs, prepositions, terminators, auxiliaries) — that vocabulary also
   ships inside the TPC-H Tools distribution under stricter terms than the specification document
   itself, and no query filters on it. Code: `crates/htap-tpch/src/generate/{rng,text}.rs`. Tests:
   `generate::rng::tests::bounded_draw_covers_range_without_returning_bound`;
   `generate::tests::generation_is_deterministic`; `generate::text::tests::{
   generated_text_respects_requested_lengths, phrase_text_places_phrase_at_varying_offsets,
   phrase_text_respects_requested_lengths_and_order}`.

6. **The O_COMMENT forced phrase for Query 13 is a mechanism of ours.** Clause 4.2.3 states only a
   length range for `O_COMMENT`; without an addition, Query 13's `NOT LIKE '%special%requests%'`
   predicate would be vacuously true for every generated row. The generator inserts a two-word phrase
   (drawn from the specification's own 2.4.13.3 query-parameter candidate lists) into a documented,
   independently-drawn fraction of orders — the insertion rate and mechanism are this kit's own, not
   specification text. Code: `crates/htap-tpch/src/generate/orders.rs`. Tests:
   `generate::orders::tests::{forced_comment_phrase_has_documented_rate_and_varied_pairs,
   forced_phrase_cohort_is_not_customer_key_derived}`.

7. **Refresh functions (RF1/RF2).** Per-order transactions are **spec-permitted by Clause 2.5.2**
   (which gives exactly this per-order shape as its own worked example) and are recorded here as
   conformance, not a deviation — see §4 below. The actual deviations are: RF1 streams 1001-3000 are
   this crate's own extension beyond Clause 4.2.4's documented 1-1000 pairing; Clause 4.2.4.3's full
   4,000-pair quarter-reuse cycle is not implemented (only one pass through slices 0-3 is supported);
   refresh-inserted rows never carry the Query 13 forced-comment phrase from item 6 above (refresh
   text generation is ordinary, not the load-time forced-phrase path); and re-running the same RF1
   stream without an intervening RF2 upserts byte-identical rows, since this engine's `INSERT`
   performs no primary-key-existence check. See ADR-029. Code: `crates/htap-tpch/src/refresh.rs`.
   Tests: `refresh::tests::{rf1_counts_and_generation_are_deterministic,
   rf1_streams_use_the_expected_sparse_slices, out_of_range_streams_are_rejected,
   rf2_stream_one_keys_match_the_load_order_key_range}`;
   `crates/htap-tpch/tests/refresh.rs::{test_rf1_inserts_orders_and_lineitems_with_correct_counts,
   test_rf2_deletes_orders_and_makes_rows_gone, test_rf1_then_rf2_restores_counts_with_quarter_shift,
   test_partial_failure_rolls_back_without_committing_half_order,
   test_out_of_range_streams_are_rejected}`.

8. **The bulk loader commits in independently committed batches, with no whole-table atomicity.**
   `load_dataset` streams each table through `LoadOptions::batch_rows`-sized batches (default 1,000
   rows); a failure partway through a table's import leaves its earlier batches committed, and the
   loader's post-load `CopyReport` check is an all-or-nothing *verification* after the fact, not an
   all-or-nothing *transaction*. Code: `crates/htap-tpch/src/load.rs::load_dataset`. Tests:
   `crates/htap-tpch/tests/load.rs::{row_counts_copy_reports_and_round_trip_values,
   second_load_rejection, invalid_batch_rows_does_not_create_tables}`.

9. **Concurrency: `htap-server`'s process-wide execution lock serializes every statement.**
   `LocalServer::execute`, `Session::execute`, and `Session::commit` each hold one process-wide
   `Mutex<()>` for the whole call (`crates/htap-server/src/lib.rs`, lines 199, 443, 455, 1210), so
   `throughput_test`'s concurrency is submission- and session-level only: statements from different
   streams never execute inside the engine at the same instant, unlike a real multi-user TPC-H
   Throughput Test. See ADR-030. Code: `crates/htap-tpch/src/drivers.rs::throughput_test`. Test:
   `crates/htap-tpch/tests/drivers.rs::throughput_driver_runs_query_and_refresh_streams` exercises the
   driver end to end; the serialization itself is a property of `crates/htap-server/src/lib.rs`
   verified by code read-through (see ADR-030) and disclosed in the module doc, the report types'
   doc comments, and here.

## 4. Conformance statements (not deviations)

These are worth stating explicitly so a reader does not mistake a correct, spec-permitted choice for
an undisclosed gap.

- **Row-limit mechanism 3 (Clause 2.1.2.9), applied to queries 2, 3, 10, 18, and 21.** The clause
  requires one of three row-limiting mechanisms, chosen and used consistently. This kit uses
  mechanism 3 — vendor-specific `SELECT`-statement syntax (`LIMIT n`) — which the clause itself states
  "is not classified as a minor query modification since it completes the functional requirements of
  the functional query definition." See ADR-027. Code: `crates/htap-tpch/src/queries.rs` (queries 2,
  3, 10, 18, 21 carry `limit 100`/`10`/`20`/`100`/`100`). Tests:
  `queries::tests::test_all_22_queries_bind_and_execute`;
  `crates/htap-server/tests/limit_truncation.rs::{limit_returns_only_the_requested_number_of_rows,
  limit_applies_after_ordering, limit_larger_than_result_returns_all_rows,
  limit_zero_returns_no_rows, limit_with_offset_skips_rows_before_truncating}` (a synthetic-table
  suite covering the truncation mechanism itself, since no TPC-H fixture's result set is large enough
  to reach its own `LIMIT`).

- **The `'AIR REG'` ship mode is the specification's own acknowledged quirk, not our error.** The
  Modes domain list (Clause 4.2.2.13) is `REG AIR, AIR, RAIL, SHIP, TRUCK, MAIL, FOB`; Query 19's
  published text filters `l_shipmode in ('AIR', 'AIR REG')`, and the specification's own Clause
  2.4.19.5 sample-output comment states:

  > Comment: The TPC recognizes that the predicates on l_shipmode include the non-existing shipmode
  > "AIR REG".

  The generator emits `REG AIR` and never `AIR REG`, and Query 19's published text is reproduced
  unaltered — in a generated dataset, only the `AIR` member of Query 19's `IN` list can ever match.
  Neither side is changed to reconcile the other. Code:
  `crates/htap-tpch/src/generate/lineitem.rs`. Test:
  `generate::lineitem::tests::shipping_domains_are_exact_and_pinned`. The hand-derived Query 19
  fixture (`crates/htap-tpch/tests/correctness_fixture.rs::test_q19`) separately gives one witness row
  with `l_shipmode = 'AIR REG'` to exercise the published query text as written — correct for testing
  the query, but a value a generated dataset never actually contains.

- **Appendix A is reproduced verbatim.** The 41-row query-order permutation table
  (`drivers::QUERY_ORDER`) is the specification's Appendix A, copied under its own copying-by-
  permission notice (§6 below), with the specification's own attribution to Moses & Oakford carried
  in the module doc. Code: `crates/htap-tpch/src/drivers.rs`. Tests: `drivers::tests::{
  test_every_row_is_permutation, test_permutation_row_coverage, test_query_order_wraparound}`.

- **The `DATE` type is mapped onto this engine's `TIMESTAMP` storage type.** `crates/htap-sql`'s
  binder treats `DataType::Date` and `DataType::Timestamp` identically
  (`crates/htap-sql/src/binder.rs`, line 399), so every TPC-H `DATE` column (`o_orderdate`,
  `l_shipdate`, `l_commitdate`, `l_receiptdate`) is stored and compared as a microsecond timestamp.
  This is a general SQL-layer behavior, not specific to this crate. Test:
  `crates/htap-server/tests/tpch_sql_prerequisites.rs::test_tpch_sql_prerequisites_parse_bind_and_execute_dates_and_strings`.

## 5. Validation evidence

- **22 hand-derived fixture tests.** `crates/htap-tpch/tests/correctness_fixture.rs::{test_q1, test_q2,
  test_q3, test_q4, test_q5, test_q6, test_q7, test_q8, test_q9, test_q10, test_q11, test_q12,
  test_q13, test_q14, test_q15, test_q16, test_q17, test_q17_zero_qualifying_rows_is_null, test_q18,
  test_q19, test_q20, test_q21, test_q22}` assert hand-derived expected row values (not just row
  counts) for all 22 queries against small, purpose-built datasets, each built so that dropping any
  single clause of its query changes the result. Command: `cargo test -p htap-tpch --test
  correctness_fixture`.

- **Independent re-aggregation oracle (task B6).** `crates/htap-tpch/tests/oracle/` re-derives each of
  the 22 queries' expected result directly from the generated `Dataset` in hand-written Rust, with no
  SQL engine and no reused engine decimal helper. All 22 queries match the engine exactly at one seed
  (42) and scale factor 0.01, **except** Query 17 and Query 18 (SF 0.03) and Query 20 (SF 0.1) — the
  published validation-default parameters give `NULL`/empty results for those three at SF 0.01, and a
  dataset-derived coverage assertion (not just the query result) is what caught that the SF-0.01
  "passes" for those queries were `NULL = NULL` or empty = empty, not real agreement. Tests:
  `crates/htap-tpch/tests/correctness_generated.rs::{test_q1_pilot, test_q2_pilot, test_q11_pilot,
  test_q17_pilot, test_q3_batch2, test_q4_batch2, test_q5_batch2, test_q6_batch2, test_q7_batch2,
  test_q8_batch2, test_q9_batch3, test_q10_batch3, test_q12_batch3, test_q13_batch3, test_q14_batch3,
  test_q15_batch3, test_q16_batch4, test_q18_batch4, test_q19_batch4, test_q20_batch4,
  test_q21_batch4, test_q22_batch4, tpch_oracle_all_22_queries}`. Command: `cargo test --release -p
  htap-tpch --test correctness_generated -- --ignored`.

- **Refresh oracle test.** `crates/htap-tpch/tests/correctness_generated.rs::
  test_refresh_rf1_rf2_stream_one_updates_q1_and_q6` applies RF1+RF2 through the engine at key stream
  1, mutates a cloned `Dataset` the same way, and checks Query 1 and Query 6 against the independent
  oracle over the mutated dataset (also asserting at least one RF1-inserted lineitem passes Query 1's
  own date filter, so the check is not vacuous). Same command as above (`-- --ignored`).

- **Power-order oracle test.** `crates/htap-tpch/tests/correctness_generated.rs::
  test_refresh_rf1_rf2_stream_one_all_queries` extends the same RF1/RF2-then-oracle check from Query
  1/Query 6 to all 22 queries in `query_order(0)` (the same order `power_test` uses), each at its own
  task-B6 scale factor, over the mutated dataset. Same command.

- **Driver and loader integration tests.** `crates/htap-tpch/tests/drivers.rs::{
  power_driver_runs_all_queries_and_refreshes, power_driver_rejects_invalid_refresh_key_stream_without_changes,
  throughput_driver_runs_query_and_refresh_streams}` and `crates/htap-tpch/tests/load.rs::
  all_22_queries` (release, `#[ignore]`d). Command: `cargo test --release -p htap-tpch --test drivers
  -- --ignored` and `cargo test --release -p htap-tpch --test load -- --ignored`.

**What this evidence does not establish:** generator conformance to the specification's own
row-generation algorithm (the generator's own 28 tests establish invariants — counts, referential
closure, domain membership, date bounds, determinism — not expected values, and it reproduces no
other implementation's PRNG sequence, by design — see item 5 above); behavior at any seed other than
42; and behavior at any scale factor other than the ones actually run above (0.01, 0.03, and 0.1). No
audited large-scale-factor run has ever been performed.

## 6. Metrics

Per TPC Policies §8.1.4, `QphH`, `QppH`, `QthH`, and `Price/Performance` (`$/QphH`) are never computed
or named anywhere in this crate — not under those names and not under a relabeled "unofficial"
variant. See ADR-030 for the full policy discussion. `power_test`/`throughput_test` instead report
only neutral diagnostics in seconds: per-query `QueryTiming` (submission interval and execute
duration) and per-refresh `RefreshTiming`, summarized by `rounded_interval_geomean_seconds` (Power
Test: the geometric mean of 24 rounded intervals — RF1, all 22 query submission intervals, RF2 — each
rounded to the nearest 0.01s with a 0.01s floor, Clause 5.3.7.5) and `measurement_interval`
(Throughput Test: earliest submission to latest completion across every stream and the refresh
thread, rounded up, Clause 5.3.6.1/5.3.6.2). Two label constants, `POWER_METRIC_LABEL` and
`THROUGHPUT_METRIC_LABEL`, are the only summary text either report type exposes; a unit test asserts
neither contains `qphh`, `qpph`, `qthh`, `price`, `performance`, `composite`, or `query-per-hour`
(case-insensitive). Test: `drivers::tests::test_labels_do_not_contain_official_metrics`.

## 7. Explicitly not claimed

- **No claim of decimal aggregate exactness at scale factors nobody has run.** ADR-026's clamp of
  derived `DECIMAL` precision to 18 digits incidentally covers TPC-H's wider "Big Decimal" notion
  (Clause 1.3.1): the 18-digit maximum comfortably holds a `SUM(l_extendedprice)`-style aggregate at
  the scale factors this kit actually runs (0.001-0.1 in its own tests). That is the claim, and it is
  not inflated to any larger, unrun scale factor. Test:
  `crates/htap-server/tests/decimal_aggregation.rs::test_tpch_style_money_aggregation_uses_exact_decimal_precision`.
- **No audit.** This crate has not been submitted to, or reviewed by, the TPC. No claim here should be
  read as a substitute for one.
- **No price/performance claim of any kind.** No cost, price, or `$/QphH`-shaped figure is computed
  anywhere in this crate.
- **No TPC-H compliance or comparability claim.** `power_test`/`throughput_test` resemble the
  specification's own test procedure but do not conform to it (§3 items 3, 4, and 9); `fixed_parameters()`
  documents published validation-default *values*, not the parameter-generation-and-validation
  algorithm. See §1's disclaimer.

## 8. Attribution

21 of the 22 published query texts (`crates/htap-tpch/src/queries.rs`) are reproduced verbatim from
the TPC Benchmark™ H Standard Specification, Revision 3.0.1 (28 April 2022); query 15 substitutes a
CTE for the specification's `CREATE VIEW`/`SELECT`/`DROP VIEW` sequence while otherwise carrying the
specification's own column names and predicate text (§3 item 1 above is the only deviation from the
published text among the 22). Appendix A's query-order permutation table
(`crates/htap-tpch/src/drivers.rs`) is reproduced verbatim with no changes. All of this is reproduced
under the specification's own copying-by-permission notice (page 5):

> All parties are granted permission to copy and distribute to any party without fee all or part of
> this material provided that: 1) copying and distribution is done for the primary purpose of
> disseminating TPC material; 2) the TPC copyright notice, the title of the publication, and its date
> appear, and notice is given that copying is by permission of the Transaction Processing Performance
> Council.

Both files' module documentation carries the notice, the publication title, its date, and the
"copying is by permission of the Transaction Processing Performance Council" statement, matching this
clause's conditions. Appendix A's own attribution to F. Moses and O. Oakford, *Tables of Random
Permutations* (Stanford University Press, 1963, pp. 52-53), is carried alongside it. Full detail,
including the copyright block reproduced verbatim, is in [`ATTRIBUTION.md`](../ATTRIBUTION.md)'s
"Transaction Processing Performance Council (TPC-H)" section — that file is the canonical attribution
record; this document does not duplicate its full text.

No TPC-H Tools distribution source (`dbgen`, `qgen`, or the reference implementation) was vendored,
copied, or machine-translated into this repository at any point (§3, item 5 above).
