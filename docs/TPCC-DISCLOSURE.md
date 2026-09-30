# Derived from TPC-C — Compliance and Deviations Disclosure

Phase 18, task 18 (batch 4). This document is the required TPC-C compliance/deviations disclosure for
`crates/htap-tpcc`, the workload kit built across Phase 18 batches 1-3 (schema, population generator, bulk
loader, the five transactions, the consistency checker, the isolation tests, the workload driver and the
SQL-literal escaping fix). It covers the TPC Policies requirements that apply to work derived from a TPC
Benchmark Standard (TPC Policies v6.20, November 2024, §8.1.4, §8.1.5, §8.3.2, §8.3.3) and the TPC Benchmark™ C
Standard Specification Revision 5.11 (February 2010, "the specification"). Per §8.1.4, this document's title and
every place this project's own workload kit is named as "the TPC-C Benchmark" carry the "Derived from" prefix; a
bare "TPC-C" elsewhere names the specification itself, one of its clauses, or repository shorthand already used
throughout this project's documentation (e.g. "TPC-C schema", "TPC-C consistency conditions", "TPC-C workload
kit"), consistent with existing usage in `docs/ARCHITECTURE.md`, `docs/LIMITATIONS.md`, and `docs/PROGRESS.md`.

This document makes no legal claim on the TPC's behalf and is not itself a TPC submission; it exists so a reader
never needs to reconstruct from code and tests alone which published behavior this kit reproduces, changes, or
omits. It mirrors [`docs/TPCH-DISCLOSURE.md`](./TPCH-DISCLOSURE.md), the Phase 17 equivalent for `crates/htap-tpch`.

## 1. Required disclaimer and non-comparability statements (verbatim)

TPC Policies §8.1.5 requires the following disclaimer, verbatim, on all work derived from a TPC Benchmark
Standard (the policy's `<TPC Benchmark Standard name>` placeholder is filled with TPC-C):

> This workload is derived from the TPC-C Benchmark and is not comparable to published TPC-C
> Benchmark results, as this implementation does not comply with all requirements of the TPC-C
> Benchmark.

TPC Policies §8.3.2 requires:

> All deviations from the TPC Benchmark Standard in question must be explicitly noted.

Section 3 below is that explicit list.

TPC Policies §8.3.3 requires:

> Results based on a non-TPC Benchmark must be clearly identified as not being comparable to an
> official TPC Benchmark Result.

Applied here: no result produced by this crate — no transaction count, no observed mix, no elapsed time, no
loaded dataset, no consistency-check outcome — is a TPC Benchmark Result or comparable to one. Nothing in this
crate has been submitted for or has passed a TPC audit.

TPC Policies §8.1.4 requires the "Derived from" naming prefix before every instance of the benchmark name in
public material describing derived work, and separately prohibits using any TPC Benchmark Standard's Primary or
Optional Metric name in derived work:

> The use of any Primary Metric or Optional Metric of a TPC Benchmark Standard in a work that is
> derived from TPC material is not allowed.

Section 6 below states how this crate satisfies that prohibition.

## 2. What this kit is

`crates/htap-tpcc` is a local diagnostic kit: an in-process schema, data-generation, bulk-load, transaction,
consistency-check, isolation-test and workload-driver library for exercising this engine against a
TPC-C-shaped workload. It is **not audited**, has gone through no TPC review process, and produces **no TPC
metric** — official or unofficially-relabeled. Nothing it computes may be quoted as a `tpmC` figure, a
price/performance figure or an availability date, or presented as comparable to one (§1 above). It is an
`implemented (local MVP)` component: a narrow local slice, not a benchmark implementation. See
`docs/LIMITATIONS.md`'s "TPC-C workload kit scope and deferred features" and `docs/ARCHITECTURE.md`'s "TPC-C
workload kit (Phase 18)" for the build history; ADR-031 and ADR-032 in `docs/DECISIONS.md` record the design
decisions; this document is the deviations/compliance record those sections point to.

## 3. Deviations (TPC Policies §8.3.2)

Each item below names the specification clause it departs from, the code, and, where one exists, a runnable
test. Items that have no dedicated test say so.

1. **No RTE, no keying time, no think time, no response-time constraints (Clauses 5.2.2, 5.2.5, 5.3, 6.4).**
   The specification's emulated-user cycle (menu, keying time, transaction, think time) and its 90th-percentile
   response-time limits require a remote terminal emulator (RTE) measuring at the terminal. This kit has no
   RTE and no terminal screens (the Clause 2 terminal I/O layouts are not implemented; transactions are Rust
   functions with typed inputs and outputs). `drivers::run` drives N threads, each with its own `Session` and
   fixed home warehouse, back to back with no waits, and applies no response-time gate. The terminal count is
   caller-chosen and is not tied to Clause 4.2.2's 10 terminals per warehouse (the ignored full-warehouse run
   uses 2 terminals on one warehouse). Code: `crates/htap-tpcc/src/drivers.rs` (module doc). Test:
   `crates/htap-tpcc/tests/drivers.rs::report_labels_do_not_claim_official_tpcc_metrics` only checks that no
   official metric name appears in a report label; no test asserts the absence of pacing.

2. **No `tpmC`, no price/performance, no availability date, no other official metric (Clauses 5.4, 5.7,
   7).** See §6. Tests: `drivers::tests::report_labels_do_not_contain_official_metrics`;
   `crates/htap-tpcc/tests/drivers.rs::report_labels_do_not_claim_official_tpcc_metrics`.

3. **Delivery runs synchronously, not in deferred mode (Clause 2.7.2).** The clause requires the Delivery
   transaction to be queued for deferred execution, control returned to the terminal independently of
   completion, and execution information recorded in a result file. This kit queues nothing, returns control
   only when the work has committed, and writes no result file; the transaction's outcome is a typed in-memory return
   value (`DeliveryResult`). Separately, and permitted by Clause 2.7.4.1's comment (up to 10 database transactions per business
   transaction), `transactions::delivery` loops over `transactions::delivery_one_district`, so each district's
   order is delivered in its own database transaction; the driver retries only the district that conflicted
   and counts one completed Delivery per card. Code: `crates/htap-tpcc/src/transactions/mod.rs`,
   `crates/htap-tpcc/src/drivers.rs`. Tests: `crates/htap-tpcc/tests/delivery.rs::{
   delivers_oldest_order_in_each_district, skips_districts_without_outstanding_orders,
   missing_order_lines_rolls_back_district, invalid_request_is_rejected}`;
   `crates/htap-tpcc/tests/isolation.rs::delivery_one_district_does_not_redeliver_a_completed_district`. The
   driver's per-district resume behaviour (retry only the conflicting district) has no dedicated test.

4. **A fixed NURand constant `C` of 157, and a `C_LAST` `C-Delta` of 0 (Clauses 2.1.6, 2.1.6.1,
   4.3.3.1).** Clause 2.1.6 makes `C` a run-time constant randomly chosen within [0..A] and requires the same
   `C` per field (`C_LAST`, `C_ID`, `OL_I_ID`) to be used by all emulated terminals; using one `C` for `C_ID`
   and one for `OL_I_ID` across all terminals is therefore conformant. Clause 2.1.6.1's `C-Load`/`C-Run`/`C-Delta`
   rule applies to `C_LAST` only: `C-Load` lies in [0..255] and `C-Delta = |C-Load - C-Run|` must lie in
   [65..119], excluding 96 and 112. This kit deviates in two ways. (a) The `C_LAST` `C-Delta` is 0: the
   population's `C_LAST` and every run use the same 157, which violates Clause 2.1.6.1. (b) `C` is the fixed
   constant 157 rather than randomly chosen, although 157 lies within [0..A] for all three fields (A is 255,
   1023 and 8191). The `C-Delta` deviation is not a per-field one. Code: `crates/htap-tpcc/src/drivers.rs` (`FIXED_C`),
   `crates/htap-tpcc/src/generate/mod.rs` (`generate_customer`). Tests:
   `generate::text::tests::nurand_stays_in_requested_range` (range only);
   `drivers::tests::generated_default_inputs_obey_ranges_and_rates`.

5. **`HISTORY` has an added surrogate primary key `h_id BIGINT` (Clauses 1.4.7, 1.4.10).** The engine requires
   a primary key on every table and the specification's `HISTORY` table has none. `history::build_h_id` packs
   a 15-bit source and a 48-bit sequence into a non-negative `BIGINT`: source 0 is the initial population
   (sequence = row index), and runtime callers use a terminal number 1..=32767 plus their own sequence, so
   distinct (source, sequence) pairs never collide. The driver caps `terminal_count` at 32767 (the 15-bit
   source); each terminal seeds its sequence from the largest existing `h_id` of its source, so consecutive runs
   on one database keep their history rows. Clause 1.4.7 permits added attributes provided they do not improve
   performance; this kit makes no performance claim, and Clause 1.4.10 exempts `HISTORY` from its relative-
   addressing prohibition. Code: `crates/htap-tpcc/src/history.rs`, `crates/htap-tpcc/src/schema.rs`. Tests:
   `history::tests::{distinct_source_sequence_pairs_produce_distinct_ids, rejects_out_of_range_values,
   accepts_boundary_values}`; `crates/htap-tpcc/tests/schema_ddl.rs::ddl_creates_expected_catalog_schema`;
   `crates/htap-tpcc/tests/drivers.rs::{consecutive_runs_preserve_history_rows,
   rejects_terminal_count_larger_than_history_terminal_range}` (the latter uses 65536; the 32767 boundary
   itself is untested).

6. **Table names `orders`, `order_line`, `new_order`.** `ORDER` is reserved, hyphens are invalid in unquoted
   names and double quotes are string literals in this MySQL dialect, so the specification's `ORDER`,
   `ORDER-LINE` and `NEW-ORDER` are renamed. Code: `crates/htap-tpcc/src/schema.rs`. Test:
   `crates/htap-tpcc/tests/schema_ddl.rs::ddl_creates_expected_catalog_schema`.

7. **Timestamp columns: integer microseconds in transactions, a fixed constant in the population.**
   The engine's `INSERT` into a `TIMESTAMP` column accepts a date string, a `DATE` literal or an integer, but not
   a date-and-time string literal (`crates/htap-sql/src/binder.rs`, the `CommonDataType::Timestamp` branch,
   lines 1884-1927). The transactions therefore take caller-supplied integer epoch microseconds for
   `O_ENTRY_D`, `H_DATE` and `OL_DELIVERY_D` and write them as integers, instead of the current system
   date-and-time the specification requires (Clauses 2.4.1.6, 2.7.1.3). The bulk loader goes through the CSV
   `COPY` path, not `INSERT` (its timestamp parser, `crates/htap-movement/src/codec.rs::parse_timestamp_text`,
   accepts a date-and-time string), and loads the population's `C_SINCE`, `H_DATE` and `O_ENTRY_D` (and the delivered
   orders' `OL_DELIVERY_D`) as the fixed constant `2000-01-01 00:00:00` rather than the current date and time
   Clause 4.3.3.1 requires; every row of one load therefore carries the same timestamp, which keeps generation
   deterministic. Tests: `crates/htap-tpcc/tests/load.rs::load_small_subset_with_referential_consistency` and
   the transaction fixture tests under `crates/htap-tpcc/tests/`; no test asserts the binder limitation
   itself.

8. **Isolation (Clause 3.4) is adapted to optimistic concurrency control; Level 3 is not claimed.** The
   engine gives each transaction a fixed snapshot and detects write-write conflicts at commit (first committer
   wins); it never blocks. **Claim (reviewed; ADR-032 has the full paragraph), exactly as narrow as the tests:**
   the assessment is conditional on all 16 tests in `crates/htap-tpcc/tests/isolation.rs` passing for the build
   under review. The suite contains nine scenarios derived from Clause 3.4.2 and seven engine-level tests.
   Clause 3.4.2 permits alternative validation techniques for non-locking schemes provided full details are
   disclosed, and that permission does not by itself establish that each adaptation meets every original test
   objective. For the exercised schedules, the tests demonstrate exclusion of uncommitted reads (tests 1, 2, 4,
   6), rejection of conflicting commits (first committer wins, with `TransactionError::Conflict` retried from
   `BEGIN`; tests 3 and 5), rollback non-interference on the asserted state (tests 4 and 6), and fixed-snapshot
   reads (tests 7-9). Deterministic lost-update evidence comes from tests 3 (`D_NEXT_O_ID`) and 5 (customer
   balance); the four barrier-released concurrency tests are smoke tests and do not guarantee transaction
   overlap. Tests 7-9 check snapshot reads, not serializability; test 7 checks explicit price reads across a
   concurrent price update and does not execute a complete New-Order across the change. Prevention of the broad
   P2/P3 phenomena (Berenson et al.) and of A5B write skew is not claimed, nor is ANSI Level 3 or
   serializability, and Clause 3.4.1's Requirement 1 (Level 3 between New-Order, Payment, Delivery and
   Order-Status) is therefore not claimed to be met; no serializable mode exists. Transactions interleave
   statement by statement. The results rely on atomic commit/rollback, logical-key write-conflict detection
   including absent-key inserts, and retries at the actual transaction boundary (whole transaction for
   New-Order, Payment, Order-Status and Stock-Level; per district for Delivery). `INSERT` has upsert semantics
   and does not reject duplicate keys. See ADR-032. Code:
   `crates/htap-tpcc/tests/isolation.rs`. Tests: `isolation_test_1_new_order_then_order_status`,
   `isolation_test_2_rollback_is_not_visible_to_order_status`,
   `isolation_test_3_two_new_orders_have_consecutive_ids`,
   `isolation_test_4_failed_new_order_does_not_consume_order_id`,
   `isolation_test_5_delivery_then_payment_updates_both_values`,
   `isolation_test_6_rolled_back_delivery_has_no_customer_effect`,
   `isolation_test_7_new_order_uses_a_fixed_price_snapshot`,
   `isolation_test_8_delivery_snapshot_has_no_phantom_new_order`,
   `isolation_test_9_order_status_snapshot_has_no_order_phantom`. Scope of each: tests 1-6 hand-stage the
   uncommitted half of the scenario as raw SQL inside an explicit transaction (the transaction functions commit
   internally), with T1 staged and open while T2 runs a full transaction; all of tests 1-9 run
   deterministically on one thread with no sleeps; test 4 detects dirty-write and abort-leak models, not
   conflict detection; test 7 is different: it uses three sessions (T1 and T2 read prices in explicit
   transactions, T3 commits a price update) and checks fixed-snapshot price reads with plain `SELECT`s, not a
   full New-Order across the change (a New-Order runs only afterwards, on a fresh snapshot); tests 8 and 9 use
   two sessions and read-only snapshots.

9. **Tests 7-9 check snapshot reads, and the four concurrency tests are smoke tests only.** Isolation tests 8
   and 9 assert that a transaction sees no new `NEW-ORDER` row and no newer `ORDER` within its own snapshot, and
   test 7 that explicit price reads keep returning the old prices after a concurrent price update; those are
   snapshot-read outcomes, not serializability tests. The four
   barrier-released concurrency tests — `two_concurrent_new_orders_same_district`,
   `concurrent_payments_same_customer`, `two_concurrent_deliveries_same_warehouse`,
   `concurrent_new_orders_same_stock_decrement` — each assert their final state (no duplicate `O_ID`, no lost
   Payment update, no double delivery, no lost stock decrement) after threads retry on `Conflict`, but they only
   release threads from a barrier and nothing forces the snapshots to overlap, so they are smoke/stress tests,
   not deterministic evidence.

10. **ACID atomicity, consistency and durability tests are not performed as the specification defines them
    (Clauses 3.2, 3.3.3, 3.5).** What exists, exactly: (a) the consistency checker for all 12 Clause 3.3.2
    conditions, run after load and after transactions (the specification demonstrates only conditions 1-4
    explicitly); its deliberate-corruption tests cover conditions 1-4 only, conditions 5-12 are exercised on
    consistent data; (b) per-transaction rollback behaviour exercised by the transaction fixture tests
    (`unused_item_rollback`, `missing_order_lines_rolls_back_district`, `missing_customer_rolls_back`) and by
    isolation tests 2, 4 and 6; and (c) the engine's own crash-recovery and durability tests elsewhere in the
    workspace (see `docs/PROGRESS.md`), which are not TPC-C tests. **Not performed:** Clause 3.2.2's Payment
    atomicity tests as written, Clause 3.3.3.2's five-minute driven consistency test at 90% of a reported rate,
    and every Clause 3.5.3 durability failure test (instantaneous interruption, loss of memory, power failure,
    media failure). This kit contains no TPC-C durability test and claims none. Code:
    `crates/htap-tpcc/src/consistency.rs`. Tests: `crates/htap-tpcc/tests/consistency_small.rs::{
    test_small_warehouse_all_conditions_pass, test_small_warehouse_conditions_after_transactions,
    test_small_warehouse_corruption_condition_1, test_small_warehouse_corruption_condition_2,
    test_small_warehouse_corruption_condition_3, test_small_warehouse_corruption_condition_4}`;
    `crates/htap-tpcc/tests/consistency.rs::{unknown_warehouse_is_rejected,
    warehouse_without_districts_reports_ytd_violations}`; ignored full-warehouse versions in §5.

11. **Consistency condition 11 holds only for the initial population (Clause 3.3.2.11) — a spec-text
    observation, not an engine defect.** The condition reads `count(ORDER) - count(NEW-ORDER) = 2100` per
    district. Each Delivery removes one `NEW-ORDER` row and leaves the `ORDER` row, so after any Delivery the
    difference is greater than 2100 while the database is still correct. The checker implements the condition
    as written (so it is reported after Deliveries). Two different sets of assertions cover this. After one
    hand-driven New-Order, Payment and Delivery,
    `crates/htap-tpcc/tests/consistency.rs::transactions_preserve_derived_condition_11` (ignored) and its fast
    small-dataset counterpart `crates/htap-tpcc/tests/consistency_small.rs::test_small_warehouse_conditions_after_transactions`
    assert only that district 1's `count(ORDER) - count(NEW-ORDER)` equals 2100 plus the number of orders that
    test's Delivery delivered and that the violation set is exactly {11}. After the workload driver's runs, the shared
    helper `assert_condition_11_and_delivery_invariant` in `crates/htap-tpcc/tests/drivers.rs` (used by
    `run_small_consistent_dataset`, `consecutive_runs_preserve_history_rows` and
    `run_full_warehouse_load_ignored`) asserts the derived per-district invariant `count(ORDER) -
    count(NEW-ORDER) = count(ORDER with O_CARRIER_ID)`, requires condition 11 to be reported if and only if that
    difference is not 2100, and allows no other condition to be violated. The carrier-count invariant and the
    "reported if and only if" rule are asserted only by that driver helper.

12. **Driver starvation escalation to exclusive execution.** Without think time, long New-Orders starved under
    first-committer-wins on the hot warehouse and district rows (the engine's messages were diagnosed as
    genuine write-write conflicts, not an engine bug). After 3 conflicts (`ESCALATION_CONFLICT_THRESHOLD`) the
    driver runs that transaction on the exclusive side of a driver-level `RwLock`; a transaction that still
    conflicts after 10 retries (`MAX_CONFLICT_RETRIES`) fails the run. This is a driver liveness mechanism, not
    an engine property: the engine stays optimistic. It is also unrelated to any specification mechanism. No
    test asserts escalation specifically: `run_small_consistent_dataset` only asserts that conflict retries plus
    escalations were greater than zero. Code: `crates/htap-tpcc/src/drivers.rs`. Test:
    `crates/htap-tpcc/tests/drivers.rs::run_small_consistent_dataset`.

13. **`LocalServer`'s process-wide execution lock serializes every statement.** `LocalServer::execute`,
    `Session::execute` and `Session::commit` each hold the one process-wide `execution_lock` for the whole
    call, which is held per call, not across a transaction. The lock is a field of `LocalServer`
    (`crates/htap-server/src/lib.rs`, taken in `LocalServer::execute`); the session paths take it in
    `crates/htap-server/src/session.rs` (`Session::execute` reaches it through `Session::execute_statement`, and
    `Session::commit` takes it directly). Transactions
    from different terminals can therefore interleave statement by statement, but statements never run
    inside the engine in parallel, so terminal concurrency is submission- and session-level only. See
    ADR-030 for the same finding on the TPC-H driver. Evidence is a code read-through; the driver's end-to-end
    tests exercise it only indirectly.

14. **Transaction mix by a 23-card deck, guaranteed only per completed deck (Clauses 5.2.3, 5.2.4.2).**
    Each terminal shuffles its own deck of 23 cards (10 New-Order, 10 Payment, 1 each Order-Status, Delivery,
    Stock-Level), which is the Clause 5.2.4.2 technique (a deck comprises one or more sets of 23 cards, each
    pass in a new random order). The minima (Payment 43.0%, Order-Status, Delivery and Stock-Level 4.0%; no
    New-Order minimum) are guaranteed only across completed decks: a transaction limit is a counter shared across
    terminals and can stop a run mid-deck. The observed mix counts committed transactions only and excludes
    expected New-Order rollbacks. `assert_report` in `tests/drivers.rs` therefore keeps 1 percentage point of
    slack below the deck's exact shares (10/23 = 43.48% and 1/23 = 4.35%), which lowers the asserted floors to
    42.48% and 3.35%; those are below the specification's 43.0% and 4.0% minima, so the test does not by itself
    prove a mix at or above the minima on a run cut mid-deck (a full deck does meet them). There is no
    measurement interval (Clause 5.5), so the minima are not evaluated over one.
    Code: `crates/htap-tpcc/src/drivers.rs`. Tests:
    `drivers::tests::deck_has_required_composition_and_minimum_mix`;
    `crates/htap-tpcc/tests/drivers.rs::run_small_consistent_dataset` (via `assert_report`).

15. **Scale: only one warehouse is exercised.** `DriverScale` and the generator follow the specification's
    per-warehouse cardinalities by default (10 districts, 3,000 customers per district, 100,000 items), but the
    tests exercise a hand-built small dataset and one full warehouse (about 600k rows: 100k `ITEM`, 100k
    `STOCK`, 30k each of `CUSTOMER`, `HISTORY` and `ORDER`, about 300k `ORDER-LINE`, 9k `NEW-ORDER`). No test
    loads or runs more than one warehouse; smaller `DriverScale` values exist for functional tests only and are
    not compliant configurations. The 60-day space computation (Clause 4.2.3) is not performed.

16. **Resource and time observations, not metrics.** A full one-warehouse load plus the consistency checker
    peaked at **20.7 GiB** (measured on 2026-09-30 in a `MemoryMax=24G` systemd scope, for
    `test_full_warehouse` and `transactions_preserve_derived_condition_11`; the same tests were OOM-killed under
    6 GiB and 12 GiB caps, and uncapped runs pushed the host into kernel OOM kills of unrelated processes). The
    full-warehouse driver run `run_full_warehouse_load_ignored` passed on 2026-09-30 in release mode, alone in a
    24G scope: load 60 s, 400 transactions over 2 terminals in 851 s, 1020 s in total. These are **observed
    numbers from one host, one run**, not performance metrics, not a rate, and not comparable to anything; they
    are quoted only to explain why the full-warehouse tests are `#[ignore]`d. The engine-side cause of the
    memory footprint (load path, or the checker's large joins and aggregations) is not yet profiled; engine
    follow-ups (F7, engine memory usage on a TPC-C one-warehouse workload) are pending.

17. **SQL literals are escaped for the MySQL dialect.** The dialect treats a backslash as an escape and
    generated a-strings contain both backslashes and quotes, so `transactions::sql_literal` escapes `\` (first)
    and `'`. This is an implementation detail, not a specification departure, and is recorded because the
    defect was found by the full-warehouse driver run. Tests:
    `crates/htap-tpcc/tests/payment.rs::bad_credit_data_with_backslashes_and_quotes_round_trips` (exercises
    the crate helper through Payment's `C_DATA` update);
    `crates/htap-tpcc/tests/string_escaping.rs::{unescaped_trailing_backslash_fails_to_insert,
    unescaped_backslash_quote_is_reinterpreted_by_parser, escaped_literals_round_trip_exactly}` (check the
    escaping scheme against the engine with the test file's own copy of the helper, not the crate function; the
    middle test shows unescaped `backslash\'quote` is accepted and silently stored as `backslash'quote`).

18. **`INSERT` is an upsert; duplicate-key rejection is not enforced.** The engine's `INSERT` performs no
    primary-key-existence check. New order IDs are protected structurally: each New-Order reads and increments
    `D_NEXT_O_ID` in the same transaction, and two transactions that read the same value conflict at commit.
    Tests: `crates/htap-tpcc/tests/isolation.rs::{two_inserts_same_absent_pk, insert_over_committed_key_is_upsert}`
    (two overlapping inserts of the same absent key commit exactly once; an insert over a committed key
    overwrites it) and isolation test 3.

19. **The bulk loader commits in independently committed batches, with no whole-load rollback.**
    `load::load_dataset` streams each table through the CSV `COPY` path in batches of
    `LoadOptions::batch_rows`; a failure partway leaves the created tables and earlier batches committed, and a
    retry then fails the fresh-target check. The default CI load test covers a referentially consistent subset
    only; the full one-warehouse load is `#[ignore]`d. Code: `crates/htap-tpcc/src/load.rs`. Tests:
    `crates/htap-tpcc/tests/load.rs::{load_small_subset_with_referential_consistency,
    reject_load_when_tables_already_exist, reject_invalid_batch_size_before_ddl}` and (ignored)
    `load_with_warehouse_count_1_and_verify_counts_and_values`.

20. **The generator is an original from-specification implementation with its own PRNG; some text fields use a
    wider alphabet than the specification names (Clauses 4.3.2.1, 4.3.2.2, 4.3.3.1).** **(a)** It uses a
    hand-rolled SplitMix64 PRNG; the specification names no algorithm and permits pregenerated random numbers for
    the initial population (Clause 4.3.2.1's comment), so no other implementation's sequence is reproduced.
    **(b)** `a_string` draws from a fixed 95-character printable-ASCII alphabet: the 26 lowercase and 26 uppercase
    letters, the digits, punctuation (including `\`, `'` and `"`) and the space. Clause 4.3.2.2 defines an a-string
    as alphanumeric; its comment requires a character set able to represent at least 128 characters and
    including letters and digits. Whether a 95-character generation alphabet meets that comment is a reading
    this document does not settle; it is disclosed as a possible departure. The punctuation is deliberate: it is
    what exposed the backslash-escaping defect in item 17. **(c)** Fields the specification describes as "a-string
    of 24 letters" (`S_DIST_01..10`, `OL_DIST_INFO`) are generated as 24-character a-strings from that same
    alphabet, not letters only; `W_STATE`, `D_STATE`, `C_STATE` are two uppercase letters. Code:
    `crates/htap-tpcc/src/generate/{rng,text,mod}.rs`. Tests: `generate::rng::tests::{fixed_seed_is_deterministic,
    decimal_range_is_inclusive}`; `generate::text::tests::{string_generators_obey_length_and_character_rules,
    text_generation_is_deterministic}`; `generate::tests::population_is_deterministic_and_referentially_closed`.
    The generator tests establish invariants (cardinalities, referential closure, ranges, determinism), not
    expected values.

21. **Specification clauses this kit does not address at all.** The measurement interval and checkpoint rules
    (Clause 5.5), required reporting (5.6), the SUT, driver and communications definitions (Clause 6), pricing
    (Clause 7), the Full Disclosure Report (Clause 8) and the audit (Clause 9) have no counterpart here. Nothing in
    this repository is a Full Disclosure Report or an Executive Summary.

## 4. Conformance statements (not deviations)

These are worth stating explicitly so a reader does not mistake a correct, spec-permitted choice for an
undisclosed gap. Population conformance items were corrected after the batch 1 external review.

- **Population rules (Clause 4.3.3.1).** Cardinalities per warehouse (100,000 `STOCK`; 10 `DISTRICT`; 3,000
  `CUSTOMER` and one `HISTORY` row per customer; 3,000 `ORDER` per district; 900 `NEW-ORDER` per district, orders
  2101-3000), the `D_NEXT_O_ID` of 3001, the delivered boundary (orders 1-2100 carry an `O_CARRIER_ID` and a
  delivery date and an `OL_AMOUNT` of 0.00; orders 2101-3000 are undelivered with `NULL` carrier and delivery
  date), `O_C_ID` taken from a random permutation of 1..3000, the first 1,000 `C_LAST` values iterating 0..999
  and the remaining 2,000 drawn with NURand(255,0,999), population `OL_I_ID` uniform (not NURand), and `C_MIDDLE`
  of `OE`. Tests: `generate::tests::{population_is_deterministic_and_referentially_closed,
  order_customer_ids_are_permutations, item_and_stock_original_rates_and_cardinalities}`;
  `crates/htap-tpcc/tests/load.rs::load_with_warehouse_count_1_and_verify_counts_and_values` (ignored).
- **Decimal scales follow Clause 1.3.1.** Taxes and the customer discount are stored and generated at four
  decimal places (`DECIMAL(4, 4)`; `W_TAX` and `D_TAX` in [0.0000 .. 0.2000], `C_DISCOUNT` in [0.0000 ..
  0.5000]); money columns use two (`DECIMAL(12, 2)`, `DECIMAL(6, 2)`, `DECIMAL(5, 2)`); the loader writes the
  same scales. The mapping of the specification's numeric and date types to `DECIMAL` and `TIMESTAMP` is a
  schema decision (ADR-031). Tests: `generate::tests::taxes_and_discounts_use_four_decimal_precision`;
  `crates/htap-tpcc/tests/schema_ddl.rs::ddl_creates_expected_catalog_schema`.
- **`C_PHONE` is a 16-digit n-string, `I_DATA` and `S_DATA` are 26-50 characters, and "ORIGINAL" is placed in
  about 10% of rows (Clause 4.3.3.1).** Each `ITEM` and `STOCK` row is chosen independently with probability
  1/10 and the eight characters are written at a random offset, so the realised rate varies; Clause 4.3.3.1's
  own comment allows 5% variation for `I_DATA`, `S_DATA` and `C_CREDIT`, and `C_CREDIT = 'BC'` is chosen the same
  way (about 10%). The first two lengths and the 16 digits were fixed after the batch 1 review. Tests:
  `generate::tests::{item_and_stock_original_rates_and_cardinalities,
  customer_bad_credit_rate_is_approximately_ten_percent}`.
- **`C_LAST` syllables and zip codes.** The ten syllables of Clause 4.3.2.3 and its two worked examples
  (371 and 40) are reproduced in `generate::text`; zip codes are a four-digit random prefix plus `11111`
  (Clause 4.3.2.7). Tests: `generate::text::tests::{customer_last_name_uses_required_syllables,
  zip_code_has_numeric_prefix_and_required_suffix}`.
- **Per-district Delivery transactions are spec-permitted (Clause 2.7.4.1's comment).** See §3 item 3.
- **Mix technique (Clause 5.2.4.2).** A deck of one 23-card set, shuffled anew on every pass, is the technique
  the clause describes. See §3 item 14 for its limits.
- **The expected New-Order rollback is a distinct outcome.** New-Order rolls back for the specification's 1%
  unused-item input (Clause 2.4.1.4) and reports it as `TransactionError::ExpectedRollback`, separate from a
  `Conflict` (retried) and from other errors. By design every transaction function rolls back its open database
  transaction on an error path; the tests exercise specific such paths (§3 item 10), not every path. Tests:
  `crates/htap-tpcc/tests/new_order.rs::unused_item_rollback`;
  `drivers::tests::generated_default_inputs_obey_ranges_and_rates` (checks that the generated inputs include the
  1% invalid-item case at the specified rate).
- **`DATE` and `TIMESTAMP` share one engine type.** `crates/htap-sql`'s binder treats the two identically, so every
  TPC-C date column is stored as a microsecond timestamp; this is general SQL-layer behaviour, also noted in
  `docs/TPCH-DISCLOSURE.md`.

## 5. Validation evidence

Test names below were checked against `cargo test -p htap-tpcc -- --list` and
`cargo test -p htap-tpcc -- --list --ignored`.

- **Schema and generator.** `crates/htap-tpcc/tests/schema_ddl.rs::ddl_creates_expected_catalog_schema`; unit
  tests `generate::tests::{customer_bad_credit_rate_is_approximately_ten_percent,
  item_and_stock_original_rates_and_cardinalities, order_customer_ids_are_permutations,
  population_is_deterministic_and_referentially_closed, taxes_and_discounts_use_four_decimal_precision}`,
  `generate::rng::tests::{decimal_range_is_inclusive, fixed_seed_is_deterministic}`,
  `generate::text::tests::{customer_last_name_uses_required_syllables, nurand_stays_in_requested_range,
  permutation_is_a_bijection, string_generators_obey_length_and_character_rules, text_generation_is_deterministic,
  zip_code_has_numeric_prefix_and_required_suffix}` and `history::tests::{accepts_boundary_values,
  distinct_source_sequence_pairs_produce_distinct_ids, rejects_out_of_range_values}`. Command: `cargo test -p
  htap-tpcc --lib` and `cargo test -p htap-tpcc --test schema_ddl`.

- **Loader.** `crates/htap-tpcc/tests/load.rs::{load_small_subset_with_referential_consistency,
  reject_load_when_tables_already_exist, reject_invalid_batch_size_before_ddl}` (default) and
  `load_with_warehouse_count_1_and_verify_counts_and_values` (ignored; release, about 67 s). Command:
  `cargo test -p htap-tpcc --test load`.

- **Transactions (hand-built fixtures, each built so that dropping any clause changes the result).**
  `tests/new_order.rs::{all_home_lines, both_stock_rules, brand_generic, exact_amount, one_step_total_rounding,
  remote_lines, stock_below_threshold, unused_item_rollback}`; `tests/payment.rs::{by_id_good_credit,
  by_id_bad_credit, by_last_odd_count, by_last_even_count, remote_customer, c_data_truncation,
  bad_credit_data_with_backslashes_and_quotes_round_trips}`; `tests/order_status.rs::{selects_most_recent_order,
  last_name_selects_lower_median_customer, missing_customer_rolls_back, invalid_customer_selector_is_rejected}`;
  `tests/delivery.rs::{delivers_oldest_order_in_each_district, skips_districts_without_outstanding_orders,
  missing_order_lines_rolls_back_district, invalid_request_is_rejected}`; `tests/stock_level.rs::{
  counts_distinct_low_stock_items_from_last_twenty_orders, excludes_other_districts_and_warehouses,
  missing_stock_rows_are_reported, missing_district_is_reported, invalid_threshold_is_rejected}`. Command:
  `cargo test -p htap-tpcc --test new_order --test payment --test order_status --test delivery --test
  stock_level`.

- **Consistency checker.** Fast: `tests/consistency_small.rs::{test_small_warehouse_all_conditions_pass,
  test_small_warehouse_conditions_after_transactions, test_small_warehouse_corruption_condition_1,
  test_small_warehouse_corruption_condition_2, test_small_warehouse_corruption_condition_3,
  test_small_warehouse_corruption_condition_4}` (one targeted corruption each for conditions 1-4, asserting the
  exact codependent violation sets {1,8}, {2}, {3,5,11}, {4,6}) and `tests/consistency.rs::{
  unknown_warehouse_is_rejected, warehouse_without_districts_reports_ytd_violations}`. Ignored, one full
  warehouse each: `tests/consistency.rs::{load_and_verify_all_conditions_pass,
  transactions_preserve_derived_condition_11, corruption_of_condition_1_reports_only_condition_1,
  corruption_of_condition_2_reports_only_condition_2, corruption_of_condition_3_reports_only_condition_3,
  corruption_of_condition_4_reports_only_condition_4, test_full_warehouse}` (7/7 pass in release, about
  985 s in total). Corruption tests exist for conditions 1-4 only.

- **Isolation.** `tests/isolation.rs` (16 default tests): the nine adapted Clause 3.4.2 tests, the four
  concurrency smoke tests, the two insert-conflict tests and
  `delivery_one_district_does_not_redeliver_a_completed_district`, all named in §3 items 3, 8, 9 and 18. The
  isolation assessment is conditional on all 16 passing (§3 item 8). Command:
  `cargo test -p htap-tpcc --test isolation`.

- **Driver.** `drivers::tests::{deck_has_required_composition_and_minimum_mix,
  generated_default_inputs_obey_ranges_and_rates, report_labels_do_not_contain_official_metrics}`;
  `tests/drivers.rs::{run_small_consistent_dataset, rejects_zero_warehouse_count, rejects_zero_terminal_count,
  rejects_terminal_count_larger_than_history_terminal_range, rejects_zero_transaction_limit,
  rejects_zero_duration_limit, rejects_zero_scale_dimension, report_labels_do_not_claim_official_tpcc_metrics}`
  (fast) plus three API-shape checks (`transaction_kinds_have_distinct_debug_names,
  transaction_kinds_have_expected_count, duration_limit_accepts_nonzero_duration`) that are not evidence of
  behaviour; ignored: `consecutive_runs_preserve_history_rows` (about 36 s in debug; ignored for time) and
  `run_full_warehouse_load_ignored` (generate, load, 2 terminals, 400 transactions).

- **How to run the ignored full-warehouse tests.** They are `#[ignore]`d because a full warehouse plus the
  checker needs about 20.7 GiB (§3 item 16). Run them one at a time, in release, inside a memory-capped scope so
  an out-of-memory condition kills the test rather than unrelated processes; for example
  `systemd-run --user --scope -p MemoryMax=24G cargo test --release -p htap-tpcc --test consistency --
  --ignored --test-threads=1`, and the same shape for `--test drivers -- --ignored run_full_warehouse_load_ignored`
  and `--test load -- --ignored`. The cap value and the release build match the recorded runs; the exact
  `systemd-run` flags are the standard form and are not part of a recorded result.

**What this evidence does not establish:** generator conformance to the specification's own random-generation
semantics beyond the invariants above; behaviour at any seed or warehouse count other than those run; that
escalation, the driver's per-district Delivery resume, the 32767 terminal-count boundary or setup-before-barrier
ordering behave as designed (none has a dedicated test); serializability, ANSI Level 3 isolation, prevention of
the broad P2/P3 phenomena or of A5B write skew (none is claimed; §3 item 8); that each Clause 3.4.2 adaptation
meets every original test objective; any Clause 3.2/3.3.3/3.5 ACID test outcome; and any throughput or response-time
property. No audited run has ever been performed.

## 6. Metrics

Per TPC Policies §8.1.4 and Clause 5.7.1, `tpmC` (the Performance Metric, Clause 5.4.3), `price/tpmC` (the
Price/Performance Metric) and the availability date, together with the optional `watts/KtpmC` energy metric,
are never computed or named anywhere in this crate — not under those names and not under a relabeled
"unofficial" variant. `drivers::run` reports only neutral counts and one duration: the completed-transaction
count, the expected-rollback count, the conflict-retry count, the escalation count, the observed mix percentage
per transaction type and the elapsed duration. The six label constants (`COMPLETED_LABEL`,
`EXPECTED_ROLLBACKS_LABEL`, `CONFLICT_RETRIES_LABEL`, `ESCALATIONS_LABEL`, `OBSERVED_MIX_LABEL`,
`ELAPSED_LABEL`) are free `pub const`s in `crates/htap-tpcc/src/drivers.rs`. `WorkloadReport` carries only
numeric fields (the per-type transaction counts and observed percentages, and the elapsed `Duration`); it holds
no label text and has no `Display` implementation, so nothing ties the constants to the report's output. Two
tests scan the six constants only, asserting that none contains `tpmc`, `tpm-c`, `transactions per minute`,
`performance` or `price` (case-insensitive). No rate (transactions per unit of time) is computed. Tests: `drivers::tests::report_labels_do_not_contain_official_metrics`;
`crates/htap-tpcc/tests/drivers.rs::report_labels_do_not_claim_official_tpcc_metrics`. The label tests cover the
label constants only (not any report output, since the report carries no label text); they cannot prevent a caller dividing the counts by the elapsed duration, which would still
not be a `tpmC` figure or comparable to one. The 400-transactions-in-851-seconds observation in §3 item 16 is a
recorded elapsed time, not a throughput metric.

## 7. Explicitly not claimed

- **No audit.** This crate has not been submitted to, or reviewed by, the TPC. No claim here should be read as a
  substitute for one.
- **No `tpmC`, price/performance or availability-date claim of any kind.** No cost, price or rate figure is
  computed anywhere in this crate.
- **No TPC-C compliance or comparability claim.** `drivers::run` resembles the specification's transaction mix
  but does not conform to it (§3 items 1, 3, 4, 12, 14 and 15). See §1's disclaimer.
- **No serializability, no Level 3 isolation, no ACID-compliance claim.** See §3 items 8, 9 and 10.
- **No durability claim for TPC-C.** The engine's own crash-recovery tests are engine tests; this kit has no
  Clause 3.5 durability test.
- **No claim beyond one warehouse.** See §3 item 15.

## 8. Attribution

Apart from the copying-by-permission notice quoted below, this kit and this document reproduce no text of the
specification. The specification-defined data values it uses — the ten
`C_LAST` syllables and worked examples of Clause 4.3.2.3, and the numeric constants of Clauses 2.1.6, 4.3.3.1 and
5.2.3 — are facts of the workload definition; clause numbers are cited by number. The design is re-derived from
the specification; no TPC-C sample program (Appendix A of the specification), TPC-provided software or other
implementation's source was vendored, copied or machine-translated into this repository. The specification's
copying-by-permission notice (page 4 of TPC Benchmark™ C Standard Specification, Revision 5.11, February 2010;
cover: ©2010 Transaction Processing Performance Council) is carried for the title and date:

> TPC Benchmark™, TPC-C, and tpmC are trademarks of the Transaction Processing Performance Council.
>
> Permission to copy without fee all or part of this material is granted provided that the TPC copyright notice,
> the title of the publication, and its date appear, and notice is given that copying is by permission of the
> Transaction Processing Performance Council. To copy otherwise requires specific permission.

The canonical attribution record is [`ATTRIBUTION.md`](../ATTRIBUTION.md)'s "Transaction Processing Performance
Council (TPC-C)" section; this document does not duplicate its full text.
