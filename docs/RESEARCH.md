# Research: What We Took, What We Rejected

This document records the source study that preceded implementation. It states
what was read, which mechanisms were adopted and why, which were deliberately
rejected and why, and the general lessons that shaped the design.

No code was copied or transliterated from any source studied. See
[`../ATTRIBUTION.md`](../ATTRIBUTION.md).

---

## Sources studied

### `examples/starrocks`

StarRocks (Apache-2.0), branch `main`, commit `444cb3cf593`. Used as an
architectural reference only. Mechanisms described below were re-derived from
the designs observed; no code was copied or transliterated.

### `examples/zookeeper` — specified by the project brief but NOT PRESENT

The brief specified a second reference source at `examples/zookeeper`
containing Apache ZooKeeper source. That directory did not exist:
`examples/` contained only `starrocks/`.

ZooKeeper semantics were therefore derived from the public ZooKeeper 3.9
documentation and from the `zookeeper-async` crate, rather than from source.
To compensate for the missing source, the ZooKeeper backend is validated
against a **real ZooKeeper 3.9 ensemble running in Docker** rather than
against a mock — session expiry and ephemeral-node loss are precisely the
behaviours a hand-written mock gets wrong.

Cross-reference: [`LIMITATIONS.md`](./LIMITATIONS.md) records this missing
input, its impact, the mitigation, and the completion plan should the source
later be supplied.

---

## StarRocks: findings we adopted

### 1. Primary-key tables are "delete-and-insert", not merge-on-read

A primary index maps an encoded primary key to a 64-bit value packed as
`(rssid << 32) | rowid`, where `rssid` is a tablet-global segment id and
`rowid` is the ordinal position within that segment. On upsert, the index
lookup returns the *previous* location of that key; the old `(rssid, rowid)`
pair is appended to a **delete vector** — a Roaring bitmap of dead rowids,
maintained per segment per version.

Reads are consequently a plain UNION of all rowsets, with each segment's
delete vector subtracted by a single bitmap ANDNOT. There is no key
comparison, no merge, and no sort at read time.

Delete vectors are immutable and copy-on-write: applying a delete clones the
bitmap and bumps its version, leaving existing readers unaffected.

**Why we adopted it.** It places the entire cost of MVCC on the write and
publish path, leaving analytical scans running at full speed. That is exactly
the trade-off R5 demands.

### 2. Delete-vector storage keyed by inverted version

Delete vectors are stored under a key of the form:

```text
prefix || be(tablet_id) || be(segment_id) || be(i64::MAX - version)
```

Inverting the version means versions sort *descending* within a
`(tablet, segment)` prefix. A snapshot read is therefore one forward scan from
the prefix, stopping at the first entry whose version is `<= snapshot`.

**Why we adopted it.** It answers "give me the newest version at or below my
snapshot" in a single seek, with no secondary index to maintain.

### 3. Two-phase visibility: commit assigns a version, publish makes it readable

`BEGIN` is in-memory only and deliberately not journaled, so an abandoned
write costs zero durable I/O.

Commit assigns each touched partition a version, writes **one** journal record
containing the whole transaction state, and advances `nextVersion`. This is
the linearization point: after it, the data is durable but not yet visible.

A separate background **publish** step fans out `(partition_id, version)` to
storage nodes. It is idempotent and retryable.

Visibility is gated on **version density**: a transaction becomes visible only
when its assigned version is exactly `visibleVersion + 1` for every partition
it touched. `committedVersion` is derived as `nextVersion - 1` and is never
stored.

**Why we adopted it.** Commit latency does not depend on the slowest node, and
the density check is a very cheap serialization primitive that makes recovery
a simple replay in version order. We reuse this same machinery for the R2
storage-format swap.

### 4. Segment layout and footer trailer discipline

A segment is written strictly append-only, in this order: for each column, its
data pages followed by that column's indexes, then the next column; then the
short-key index; then the serialized footer; then a fixed **12-byte trailer**:

```text
footer_length: u32 | crc32c: u32 | magic: 4 bytes
```

Readers speculatively read a fixed-size tail of the file (default 4 KiB), so
opening a segment is one I/O in the common case. If the footer turns out to be
larger than the hint, the hint is raised and a second read issued.

**Why we adopted it.** Putting the trailer at the end makes torn writes
detectable, and the tail-read hint makes segment open a single syscall in the
common case.

### 5. Per-page zone maps coupled to an ordinal index

One zone map is kept per data page, holding `min`, `max`, `has_null` and
`has_not_null`. The four-state null encoding is worth reproducing exactly:

| `has_null` | `has_not_null` | Meaning       |
| ---------- | -------------- | ------------- |
| false      | false          | no rows       |
| true       | false          | all null      |
| false      | true           | no nulls      |
| true       | true           | mixed         |

Page indexes that survive predicate evaluation are converted to row-id ranges
through the **ordinal index**, then intersected into a sparse range. Zone maps
and the ordinal index are coupled by page index.

There is a predicate asymmetry here that is easy to get wrong:

- A **lower-bound** predicate compares against `min_or_null_value()`, which
  returns NULL if the page contains any null, and NULL sorts first. A page
  containing nulls is therefore never pruned by a lower bound.
- An **upper-bound** predicate additionally requires `!max.is_null()`, so an
  all-null page *is* pruned.

**Why we adopted it.** This is the mechanism behind R1 block skipping and the
≥90%-skip acceptance criterion.

### 6. Adaptive compression with no is-compressed flag

A page body is compressed only if the space saving exceeds a threshold
(default 10%); otherwise it is stored raw. The reader infers whether a page is
compressed purely by comparing the stored body size against the
`uncompressed_size` recorded in the footer. There is no explicit flag.

**Why we adopted it.** It is zero-cost, and it is one less field that can be
corrupted or disagree with reality.

### 7. Order-preserving composite key encoding

Keys are encoded so that byte-wise comparison equals logical comparison:

- **Integers**: flip the sign bit, then write big-endian, so signed comparison
  becomes `memcmp`.
- **Strings in a non-final key position**: escape `0x00` as `0x00 0x01` and
  terminate with `0x00 0x00`.
- **Partial-key range bounds**: use sentinel markers, so that `> k` and `>= k`
  remain distinguishable when `k` is a prefix of a longer key.

StarRocks credits this scheme to Apache Kudu; we re-derive it and record the
provenance in [`../ATTRIBUTION.md`](../ATTRIBUTION.md).

**Why we adopted it.** It makes the primary index a plain byte-ordered map
that serves both point lookups and range scans.

### 8. Encoding speculation on a sample

For a string column, the writer buffers roughly the first 10,000 values,
inserts their *hashes* into a set, and chooses dictionary encoding only if the
distinct-hash count stays below a ratio of the row count (default 0.7);
otherwise it falls back to plain encoding. Counting distinct hashes rather
than distinct values accepts a negligible collision rate in exchange for not
retaining the strings themselves.

**Why we adopted it.** It is cheap, and it avoids dictionary-encoding a
high-cardinality column.

### 9. Fencing is layered, not a single mechanism

| Mechanism | What it stops |
| --------- | ------------- |
| Consensus with majority ack | A minority partition committing. |
| **Term fence** | Becoming leader requires an atomic compare-and-set insert of `latest_epoch + 1` into a dedicated store, so exactly one node claims an epoch and losers see key-exists. |
| **Session fence** | A leader lease keyed by the consensus epoch plus a monotonic generation counter, so a lease captured in a previous term is recognizably stale. |
| **Write fence** | A counted gate around the journal: entering increments an in-flight counter under the same monitor that closes the gate, so demotion drains losslessly. |

Any ambiguity arising during a role transition deliberately aborts the process
rather than continuing half-initialized.

**Why we adopted it.** R6 requires fencing tokens against stale-leader
split-brain. We implement the term fence and session fence in the
`Coordinator` trait so that both the Raft and ZooKeeper backends inherit the
guarantee rather than each re-implementing it.

### 10. Journal-then-apply with a shared applier

Every metadata mutation takes the form `log(record, |wal| { apply_to_memory() })`
— the durable write happens first, then the in-memory mutation. Critically,
the leader's apply path and a follower's replay path run the *same* function
from the *same* record, so they cannot drift.

**Why we adopted it.** It eliminates the entire bug class where "do it" and
"log it" diverge.

### 11. Hash bucketing detail worth fixing as a contract

The bucket is computed as:

```text
crc32(concatenated per-column binary encodings) % tablet_count
```

where the modulus is the actual tablet count of that index *in that
partition*, not the table-level bucket count — so a partition created with a
different bucket count still works correctly.

StarRocks notes that the per-column byte encoding must match exactly between
frontend and backend.

**Lesson.** We fix our per-type hash encoding as an explicit, versioned
contract rather than leaving it an implementation detail.

### 12. Partition pruning micro-optimization

A partition that has never been loaded — its version is still the initial
version — is pruned for free, before any predicate work is done.

---

## StarRocks: what we deliberately rejected

1. **The six-level catalog hierarchy**
   (Table → Partition → PhysicalPartition → MaterializedIndex → Tablet →
   Replica). We collapse away `PhysicalPartition` and `MaterializedIndex`: the
   former exists to support automatic sub-partitioning, the latter to support
   rollups and materialized views. The MVP has neither, and several StarRocks
   paths already assert a 1:1 logical-to-physical mapping. We keep
   Table → Partition → Tablet → Replica. Reintroducing a physical-partition
   layer is the migration path if automatic sub-partitioning is added later.

2. **BDB JE as consensus substrate.** StarRocks delegates journal replication
   and leader election entirely to Berkeley DB JE replication groups — a
   JVM-only dependency with no Rust equivalent. We use `openraft` for the
   embedded backend, while keeping the substrate-independent epoch/lease/gate
   fencing ideas described above.

3. **C++/Java implementation.** No source was transliterated; algorithms and
   layout ideas were re-derived from the described designs.

4. **Delta Column Groups** — a `.cols` sidecar file holding only the updated
   columns, overlaid at read time. Rejected for the MVP: it adds read-time
   indirection and invalidates footer zone maps for updated non-key columns —
   StarRocks itself must disable segment-level zone-map pruning when one
   exists. We take full-row rewrite instead and accept the write-amplification
   trade-off.

5. **Segment-wide bitmap indexes.** Memory-heavy at write time, and opt-in per
   column even in StarRocks. Zone maps plus bloom filters cover the MVP's
   pruning needs.

6. **`all_dict_encoded` whole-column dictionary pushdown.** Genuinely good — it
   enables operating on dictionary codes rather than values — but it is a
   performance layer on top of already-correct execution. Deferred, not
   rejected on merit.

---

## Lessons that shaped our design

- Pay the MVCC cost at write time, not at read time.
- Make the durable decision a single record, and make the fan-out idempotent.
- Derive redundant state rather than storing it.
- Make format evolution safe by always writing the old structure and
  validating the new one against it before trusting it.
