//! Recovery behaviour of the WAL exercised through the public API only:
//! round-trip, segment rolling, torn tails, corruption, transaction atomicity
//! and GC.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use htap_common::{Row, Value, Version};
use htap_rowstore::{Lsn, Wal, WalOptions, WalRecord};
use proptest::prelude::*;

fn put(txn: u64, n: i64) -> WalRecord {
    WalRecord::Put {
        txn_id: txn,
        partition_id: n as u64 % 4,
        key: n.to_be_bytes().to_vec(),
        row: Row::new(vec![
            Value::Int64(n),
            Value::String(format!("value-{n}")),
            Value::Null,
        ]),
        version: Version::new(n as u64 + 1),
    }
}

fn commit(txn: u64, v: u64) -> WalRecord {
    WalRecord::Commit {
        txn_id: txn,
        version: Version::new(v),
    }
}

/// Segment files in `dir`, sorted by name (which is LSN order).
fn segment_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    v.sort();
    v
}

#[test]
fn segment_continuity_gap_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(512)).unwrap();
    for i in 0..100 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let files = segment_files(dir.path());
    assert!(files.len() >= 3);
    // Remove the middle segment to create a gap
    std::fs::remove_file(&files[1]).unwrap();

    let err = Wal::replay(dir.path()).unwrap_err();
    assert!(matches!(err, htap_common::HtapError::Corruption(_)));
}

#[test]
fn segment_continuity_overlap_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(512)).unwrap();
    for i in 0..100 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let files = segment_files(dir.path());
    assert!(files.len() >= 3);
    // Rename the last segment to have an LSN smaller than files[1]'s next_lsn (overlap)
    let overlapping_name = dir.path().join(format!("{:020}.wal", 1));
    std::fs::rename(&files[2], &overlapping_name).unwrap();

    let err = Wal::replay(dir.path()).unwrap_err();
    assert!(matches!(err, htap_common::HtapError::Corruption(_)));
}

#[test]
fn segment_continuity_permits_nonzero_first_retained_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(512)).unwrap();
    for i in 0..100 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let files = segment_files(dir.path());
    assert!(files.len() >= 3);
    // Remove the first segment to simulate prefix GC
    std::fs::remove_file(&files[0]).unwrap();

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.segments_read, files.len() - 1);
    assert!(!replay.records.is_empty());
}

#[test]
fn round_trip_of_every_record_variant() {
    let dir = tempfile::tempdir().unwrap();
    let recs = vec![
        put(1, 1),
        put(1, 2),
        WalRecord::Delete {
            txn_id: 1,
            partition_id: 9,
            key: b"gone".to_vec(),
            version: Version::new(4),
        },
        commit(1, 5),
        put(2, 3),
        WalRecord::Abort { txn_id: 2 },
        WalRecord::Checkpoint {
            version: Version::new(5),
        },
    ];

    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    for r in &recs {
        wal.append(r).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.truncated_at, None);
    assert_eq!(replay.segments_read, 1);
    assert_eq!(
        replay
            .records
            .iter()
            .map(|(_, r)| r.clone())
            .collect::<Vec<_>>(),
        recs
    );
    for (i, (lsn, _)) in replay.records.iter().enumerate() {
        assert_eq!(*lsn, Lsn::new(i as u64));
    }
}

#[test]
fn rolling_creates_multiple_segments_and_replay_is_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(512)).unwrap();
    for i in 0..200 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let files = segment_files(dir.path());
    assert!(files.len() >= 4, "expected several segments, got {files:?}");

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.truncated_at, None);
    assert_eq!(replay.segments_read, files.len());
    assert_eq!(replay.records.len(), 200);
    for (i, (lsn, rec)) in replay.records.iter().enumerate() {
        assert_eq!(*lsn, Lsn::new(i as u64));
        assert_eq!(*rec, put(1, i as i64));
    }
}

#[test]
fn torn_tail_at_many_truncation_points_is_never_an_error() {
    // Truncating by k bytes must cut into the tail records without ever
    // producing an error or a record that was not written.
    for k in [1u64, 2, 3, 5, 8, 13, 21, 34, 55, 89] {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        for i in 0..30 {
            wal.append(&put(1, i)).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        let path = segment_files(dir.path()).pop().unwrap();
        let full = std::fs::metadata(&path).unwrap().len();
        assert!(full > k);
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full - k).unwrap();
        f.sync_all().unwrap();

        let replay = Wal::replay(dir.path()).expect("a torn tail must not be an error");
        assert!(
            replay.records.len() < 30,
            "k={k}: truncation must lose at least the final record"
        );
        assert!(
            replay.truncated_at.is_some(),
            "k={k}: truncated_at must be reported"
        );
        assert_eq!(
            replay.truncated_at,
            Some(Lsn::new(replay.records.len() as u64)),
            "k={k}: truncated_at is the LSN the torn record would have had"
        );
        // Everything returned is exactly what was written, in order.
        for (i, (lsn, rec)) in replay.records.iter().enumerate() {
            assert_eq!(*lsn, Lsn::new(i as u64));
            assert_eq!(*rec, put(1, i as i64));
        }
    }
}

#[test]
fn truncation_inside_the_frame_header_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    for i in 0..4 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let path = segment_files(dir.path()).pop().unwrap();
    let full = std::fs::metadata(&path).unwrap().len();

    // Size of the last record's frame, obtained by writing it on its own.
    let last_frame_len = {
        let probe = tempfile::tempdir().unwrap();
        let mut w = Wal::open(WalOptions::new(probe.path())).unwrap();
        w.append(&put(1, 3)).unwrap();
        w.sync().unwrap();
        std::fs::metadata(segment_files(probe.path()).pop().unwrap())
            .unwrap()
            .len()
    };

    // Cut so that only 3 bytes of the last record's 8-byte header survive.
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(full - last_frame_len + 3).unwrap();
    f.sync_all().unwrap();

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.records.len(), 3);
    assert_eq!(replay.truncated_at, Some(Lsn::new(3)));
}

#[test]
fn a_flipped_payload_byte_stops_replay_at_that_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    for i in 0..10 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let path = segment_files(dir.path()).pop().unwrap();
    // Find the third record's payload by walking the frames.
    let mut data = Vec::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    let mut off = 0usize;
    for _ in 0..3 {
        let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 8 + len;
    }
    let flip_at = (off + 10) as u64; // inside record 3's payload

    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    f.seek(SeekFrom::Start(flip_at)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    f.seek(SeekFrom::Start(flip_at)).unwrap();
    f.write_all(&[b[0] ^ 0b0100_0000]).unwrap();
    f.sync_all().unwrap();

    let replay = Wal::replay(dir.path()).expect("bad CRC must not be an error");
    assert_eq!(replay.records.len(), 3);
    assert_eq!(replay.truncated_at, Some(Lsn::new(3)));
}

#[test]
fn a_huge_payload_len_is_corruption_not_an_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    for i in 0..3 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let path = segment_files(dir.path()).pop().unwrap();
    // A frame header claiming 4 GiB, followed by nothing. If the reader
    // believed the length it would try to allocate 4 GiB before noticing.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&u32::MAX.to_le_bytes()).unwrap();
    f.write_all(&0x1234_5678u32.to_le_bytes()).unwrap();
    f.sync_all().unwrap();

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.records.len(), 3);
    assert_eq!(replay.truncated_at, Some(Lsn::new(3)));
}

#[test]
fn corruption_in_an_early_segment_stops_the_whole_replay() {
    // Documented behaviour: we cannot tell mid-file corruption from a torn
    // tail, so everything from the first bad record on is treated as lost,
    // including later segments.
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(1)).unwrap();
    for i in 0..5 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let files = segment_files(dir.path());
    assert_eq!(files.len(), 5);
    // Corrupt the payload of segment 1 (LSN 1).
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&files[1])
        .unwrap();
    f.seek(SeekFrom::Start(9)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    f.seek(SeekFrom::Start(9)).unwrap();
    f.write_all(&[b[0] ^ 0xff]).unwrap();
    f.sync_all().unwrap();

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.truncated_at, Some(Lsn::new(1)));
    assert_eq!(
        replay.segments_read, 2,
        "replay stops after the bad segment"
    );
}

#[test]
fn committed_records_survives_an_interleaved_uncommitted_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    // txn 1 commits, txn 2 does not; their records interleave.
    wal.append(&put(1, 10)).unwrap();
    wal.append(&put(2, 20)).unwrap();
    wal.append(&put(1, 11)).unwrap();
    wal.append(&put(2, 21)).unwrap();
    wal.append_commit(&commit(1, 30)).unwrap();
    wal.append(&put(2, 22)).unwrap();
    drop(wal);

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.records.len(), 6);

    let committed = replay.committed_records();
    assert_eq!(committed.len(), 2, "only txn 1's two data records");
    assert!(
        committed
            .iter()
            .all(|(_, r)| matches!(r, WalRecord::Put { txn_id: 1, .. })),
        "markers stripped, txn 2 dropped: {committed:?}"
    );
    // LSN order preserved.
    assert!(committed[0].0 < committed[1].0);
}

#[test]
fn committed_records_drops_an_explicitly_aborted_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    wal.append(&put(1, 10)).unwrap();
    wal.append(&put(2, 20)).unwrap();
    wal.append(&put(1, 11)).unwrap();
    wal.append(&WalRecord::Abort { txn_id: 2 }).unwrap();
    wal.append_commit(&commit(1, 30)).unwrap();
    drop(wal);

    let replay = Wal::replay(dir.path()).unwrap();
    let committed = replay.committed_records();
    assert_eq!(committed.len(), 2);
    assert!(committed
        .iter()
        .all(|(_, r)| matches!(r, WalRecord::Put { txn_id: 1, .. })));
}

#[test]
fn a_transaction_torn_off_before_its_commit_is_not_replayed() {
    // The exact crash-in-the-middle case: txn 2's commit record never made it
    // to disk, so none of its data may be exposed.
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
    wal.append(&put(1, 1)).unwrap();
    wal.append_commit(&commit(1, 2)).unwrap();
    wal.append(&put(2, 3)).unwrap();
    wal.append(&put(2, 4)).unwrap();
    wal.append(&commit(2, 5)).unwrap();
    wal.sync().unwrap();
    drop(wal);

    // Chop the commit record of txn 2 in half.
    let path = segment_files(dir.path()).pop().unwrap();
    let full = std::fs::metadata(&path).unwrap().len();
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(full - 6).unwrap();
    f.sync_all().unwrap();

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.truncated_at, Some(Lsn::new(4)));
    let committed = replay.committed_records();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].1.txn_id(), Some(1));
    assert!(
        !committed.iter().any(|(_, r)| r.txn_id() == Some(2)),
        "uncommitted txn 2 must not be exposed"
    );
}

#[test]
fn gc_removes_superseded_segments_and_replay_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(400)).unwrap();

    // Versions climb with i, so an early checkpoint supersedes a prefix.
    for i in 0..60 {
        wal.append(&put(1, i)).unwrap();
    }
    wal.sync().unwrap();
    let before = segment_files(dir.path()).len();
    assert!(before > 3, "need several segments, got {before}");

    // put(1, i) carries version i+1; checkpoint at v10 covers i in 0..=9.
    let removed = wal.gc(Version::new(10)).unwrap();
    assert!(removed > 0, "some prefix must be collectable");
    let after = segment_files(dir.path()).len();
    assert_eq!(after, before - removed);

    let replay = Wal::replay(dir.path()).unwrap();
    assert_eq!(replay.truncated_at, None);
    assert_eq!(replay.segments_read, after);
    // Nothing above the checkpoint was lost, and LSNs still line up with the
    // original numbering.
    let survivors: Vec<i64> = replay
        .records
        .iter()
        .map(|(lsn, rec)| {
            assert_eq!(*rec, put(1, lsn.get() as i64));
            lsn.get() as i64
        })
        .collect();
    let first = *survivors.first().unwrap();
    assert_eq!(*survivors.last().unwrap(), 59);
    assert!(
        survivors.windows(2).all(|w| w[1] == w[0] + 1),
        "surviving LSNs must be contiguous"
    );
    for i in first..60 {
        assert!(survivors.contains(&i));
    }
    // Everything dropped was at or below the checkpoint version.
    for i in 0..first {
        assert!(Version::new(i as u64 + 1) <= Version::new(10));
    }
}

#[test]
fn opening_a_missing_directory_creates_it() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("a/b/c/wal");
    let mut wal = Wal::open(WalOptions::new(&dir)).unwrap();
    assert!(dir.is_dir());
    wal.append_commit(&commit(1, 2)).unwrap();
    assert_eq!(Wal::replay(&dir).unwrap().records.len(), 1);
}

#[test]
fn replaying_a_directory_that_does_not_exist_is_empty() {
    let base = tempfile::tempdir().unwrap();
    let replay = Wal::replay(&base.path().join("nothing-here")).unwrap();
    assert!(replay.records.is_empty());
    assert!(replay.truncated_at.is_none());
    assert_eq!(replay.segments_read, 0);
}

// A record shape generator for the property tests.
prop_compose! {
    fn any_record()(
        kind in 0u8..5,
        txn_id in 0u64..8,
        partition_id in 0u64..4,
        key in prop::collection::vec(any::<u8>(), 0..24),
        n in any::<i64>(),
        s in ".{0,32}",
        version in 1u64..1000,
    ) -> WalRecord {
        let row = Row::new(vec![Value::Int64(n), Value::String(s), Value::Null]);
        match kind {
            0 => WalRecord::Put { txn_id, partition_id, key, row, version: Version::new(version) },
            1 => WalRecord::Delete { txn_id, partition_id, key, version: Version::new(version) },
            2 => WalRecord::Commit { txn_id, version: Version::new(version) },
            3 => WalRecord::Abort { txn_id },
            _ => WalRecord::Checkpoint { version: Version::new(version) },
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Whatever the mix of records and however small the segments, replay
    /// returns exactly what was appended, in order.
    #[test]
    fn prop_round_trip(
        recs in prop::collection::vec(any_record(), 1..40),
        seg in prop::sample::select(vec![1u64, 200, 4096, 1 << 20]),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(seg)).unwrap();
        for r in &recs {
            wal.append(r).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        prop_assert_eq!(replay.truncated_at, None);
        prop_assert_eq!(replay.records.len(), recs.len());
        for (i, (lsn, rec)) in replay.records.iter().enumerate() {
            prop_assert_eq!(*lsn, Lsn::new(i as u64));
            prop_assert_eq!(rec, &recs[i]);
        }
    }

    /// Truncating the log at *any* byte offset yields a clean prefix: never an
    /// error, never a record that was not written, never a reordering.
    #[test]
    fn prop_truncation_yields_a_prefix(
        recs in prop::collection::vec(any_record(), 1..25),
        cut in 0.0f64..1.0,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        for r in &recs {
            wal.append(r).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        let path = segment_files(dir.path()).pop().unwrap();
        let full = std::fs::metadata(&path).unwrap().len();
        let new_len = (full as f64 * cut) as u64;
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(new_len).unwrap();
        f.sync_all().unwrap();

        let replay = Wal::replay(dir.path()).unwrap();
        prop_assert!(replay.records.len() <= recs.len());
        for (i, (lsn, rec)) in replay.records.iter().enumerate() {
            prop_assert_eq!(*lsn, Lsn::new(i as u64));
            prop_assert_eq!(rec, &recs[i]);
        }
        // `truncated_at` marks where replay stopped at a *partial* frame. A cut
        // that lands exactly on a record boundary ends the segment cleanly, so
        // `truncated_at` is `None` even though later records were lost — a
        // clean end is indistinguishable from an untruncated log. The property
        // is therefore: if a short read was reported at all, it is reported at
        // exactly the LSN following the last surviving record.
        if let Some(at) = replay.truncated_at {
            prop_assert_eq!(at, Lsn::new(replay.records.len() as u64));
        }

        // And no uncommitted transaction leaks through the prefix.
        let committed_ids: std::collections::HashSet<u64> = replay
            .records
            .iter()
            .filter_map(|(_, r)| match r {
                WalRecord::Commit { txn_id, .. } => Some(*txn_id),
                _ => None,
            })
            .collect();
        let aborted_ids: std::collections::HashSet<u64> = replay
            .records
            .iter()
            .filter_map(|(_, r)| match r {
                WalRecord::Abort { txn_id } => Some(*txn_id),
                _ => None,
            })
            .collect();
        for (_, rec) in replay.committed_records() {
            prop_assert!(rec.is_data());
            let id = rec.txn_id().unwrap();
            prop_assert!(committed_ids.contains(&id));
            prop_assert!(!aborted_ids.contains(&id));
        }
    }
}
