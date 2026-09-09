//! Integration tests for SST writer and reader.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use htap_common::{HtapError, Row, Value, Version};
use htap_rowstore::{
    InternalKey, MemtableEntry, SstMetadata, SstOptions, SstReader, SstWriter, ValueKind,
};
use tempfile::tempdir;

fn row(id: i64, name: &str) -> Row {
    Row::new(vec![Value::Int64(id), Value::String(name.to_string())])
}

fn put_entry(
    partition_id: u64,
    user_key: &[u8],
    version: u64,
    id: i64,
    name: &str,
) -> MemtableEntry {
    MemtableEntry {
        key: InternalKey {
            partition_id,
            user_key: user_key.to_vec(),
            version: Version::new(version),
        },
        value: ValueKind::Put(row(id, name)),
    }
}

fn del_entry(partition_id: u64, user_key: &[u8], version: u64) -> MemtableEntry {
    MemtableEntry {
        key: InternalKey {
            partition_id,
            user_key: user_key.to_vec(),
            version: Version::new(version),
        },
        value: ValueKind::Delete,
    }
}

#[test]
fn test_multi_block_round_trip_and_iter_order() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("multi_block.sst");
    let opts = SstOptions::new().with_block_bytes(256);

    let mut entries = Vec::new();
    for i in 0..100 {
        let key = format!("key_{i:04}").into_bytes();
        entries.push(put_entry(0, &key, 10, i, &format!("value_{i}")));
    }

    let meta = SstWriter::write(&sst_path, 1, entries.clone(), &opts).unwrap();
    assert_eq!(meta.id, 1);
    assert_eq!(meta.entry_count, 100);
    assert_eq!(meta.min_key, Some(entries[0].key.clone()));
    assert_eq!(meta.max_key, Some(entries.last().unwrap().key.clone()));
    assert_eq!(meta.min_version, Some(Version::new(10)));
    assert_eq!(meta.max_version, Some(Version::new(10)));

    let reader = SstReader::open(&sst_path).unwrap();
    assert_eq!(reader.metadata(), &meta);

    let read_entries: Vec<MemtableEntry> = reader.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(read_entries, entries);
}

#[test]
fn test_get_first_middle_last_block() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("get_blocks.sst");
    let opts = SstOptions::new().with_block_bytes(256);

    let mut entries = Vec::new();
    for i in 0..100 {
        let key = format!("key_{i:04}").into_bytes();
        entries.push(put_entry(0, &key, 10, i, &format!("value_{i}")));
    }

    SstWriter::write(&sst_path, 2, entries.clone(), &opts).unwrap();
    let reader = SstReader::open(&sst_path).unwrap();

    // First block
    let e_first = reader
        .get(0, b"key_0000", Version::new(10))
        .unwrap()
        .unwrap();
    assert_eq!(e_first, entries[0]);

    // Middle block
    let e_mid = reader
        .get(0, b"key_0050", Version::new(10))
        .unwrap()
        .unwrap();
    assert_eq!(e_mid, entries[50]);

    // Last block
    let e_last = reader
        .get(0, b"key_0099", Version::new(10))
        .unwrap()
        .unwrap();
    assert_eq!(e_last, entries[99]);
}

#[test]
fn test_snapshot_visibility_spanning_block_boundary() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("spanning.sst");
    // Small block size to force several versions of span_key across block boundaries
    let opts = SstOptions::new().with_block_bytes(80);

    let mut entries = Vec::new();
    // Preceding padding
    for i in 0..5 {
        let key = format!("aaa_{i}").into_bytes();
        entries.push(put_entry(0, &key, 5, i, "pad"));
    }

    // Key with multiple versions ordered version DESC
    entries.push(del_entry(0, b"span_key", 10));
    entries.push(put_entry(0, b"span_key", 8, 88, "v8"));
    entries.push(put_entry(0, b"span_key", 5, 55, "v5"));
    entries.push(put_entry(0, b"span_key", 2, 22, "v2"));

    // Subsequent padding
    for i in 0..5 {
        let key = format!("zzz_{i}").into_bytes();
        entries.push(put_entry(0, &key, 5, i, "pad"));
    }

    SstWriter::write(&sst_path, 3, entries, &opts).unwrap();
    let reader = SstReader::open(&sst_path).unwrap();

    // Snapshot before earliest version -> None
    assert_eq!(reader.get(0, b"span_key", Version::new(1)).unwrap(), None);

    // Snapshot at v2 -> v2 Put
    let e2 = reader
        .get(0, b"span_key", Version::new(2))
        .unwrap()
        .unwrap();
    assert_eq!(e2.key.version, Version::new(2));
    assert_eq!(e2.value, ValueKind::Put(row(22, "v2")));

    // Snapshot at v4 -> v2 Put
    let e4 = reader
        .get(0, b"span_key", Version::new(4))
        .unwrap()
        .unwrap();
    assert_eq!(e4.key.version, Version::new(2));

    // Snapshot at v5 -> v5 Put
    let e5 = reader
        .get(0, b"span_key", Version::new(5))
        .unwrap()
        .unwrap();
    assert_eq!(e5.key.version, Version::new(5));
    assert_eq!(e5.value, ValueKind::Put(row(55, "v5")));

    // Snapshot at v7 -> v5 Put
    let e7 = reader
        .get(0, b"span_key", Version::new(7))
        .unwrap()
        .unwrap();
    assert_eq!(e7.key.version, Version::new(5));

    // Snapshot at v8 -> v8 Put
    let e8 = reader
        .get(0, b"span_key", Version::new(8))
        .unwrap()
        .unwrap();
    assert_eq!(e8.key.version, Version::new(8));
    assert_eq!(e8.value, ValueKind::Put(row(88, "v8")));

    // Snapshot at v9 -> v8 Put
    let e9 = reader
        .get(0, b"span_key", Version::new(9))
        .unwrap()
        .unwrap();
    assert_eq!(e9.key.version, Version::new(8));

    // Snapshot at v10 -> Delete tombstone (load-bearing!)
    let e10 = reader
        .get(0, b"span_key", Version::new(10))
        .unwrap()
        .unwrap();
    assert_eq!(e10.key.version, Version::new(10));
    assert_eq!(e10.value, ValueKind::Delete);

    // Snapshot at v20 -> Delete tombstone
    let e20 = reader
        .get(0, b"span_key", Version::new(20))
        .unwrap()
        .unwrap();
    assert_eq!(e20.key.version, Version::new(10));
    assert_eq!(e20.value, ValueKind::Delete);
}

#[test]
fn test_visible_tombstone_returns_some_delete() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("tombstone.sst");
    let opts = SstOptions::new();

    let entries = vec![del_entry(0, b"deleted_key", 5)];
    SstWriter::write(&sst_path, 4, entries, &opts).unwrap();

    let reader = SstReader::open(&sst_path).unwrap();
    let result = reader.get(0, b"deleted_key", Version::new(10)).unwrap();
    assert!(result.is_some());
    let entry = result.unwrap();
    assert_eq!(entry.value, ValueKind::Delete);
    assert_eq!(entry.key.version, Version::new(5));
}

#[test]
fn test_absent_key_returns_ok_none() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("absent.sst");
    let opts = SstOptions::new();

    let entries = vec![
        put_entry(0, b"k20", 5, 20, "v20"),
        put_entry(0, b"k40", 5, 40, "v40"),
    ];
    SstWriter::write(&sst_path, 5, entries, &opts).unwrap();

    let reader = SstReader::open(&sst_path).unwrap();

    // Key before min_key
    assert_eq!(reader.get(0, b"k10", Version::new(10)).unwrap(), None);
    // Key between k20 and k40
    assert_eq!(reader.get(0, b"k30", Version::new(10)).unwrap(), None);
    // Key after max_key
    assert_eq!(reader.get(0, b"k50", Version::new(10)).unwrap(), None);
    // Non-existent partition
    assert_eq!(reader.get(1, b"k20", Version::new(10)).unwrap(), None);
}

#[test]
fn test_bloom_filter_no_false_negatives() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("bloom.sst");
    let opts = SstOptions::new().with_bloom_bits_per_key(10);

    let count = 1000;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let key = format!("user_{i:06}").into_bytes();
        entries.push(put_entry(0, &key, 1, i as i64, "bloom_test"));
    }

    SstWriter::write(&sst_path, 6, entries.clone(), &opts).unwrap();
    let reader = SstReader::open(&sst_path).unwrap();

    // Assert every single inserted key is found (zero false negatives)
    for entry in &entries {
        let found = reader
            .get(
                entry.key.partition_id,
                &entry.key.user_key,
                Version::new(10),
            )
            .unwrap();
        assert!(found.is_some(), "false negative for key {:?}", entry.key);
    }

    // Verify negative lookups actually return None
    for i in count..(count + 100) {
        let key = format!("user_{i:06}").into_bytes();
        assert_eq!(
            reader.get(0, &key, Version::new(10)).unwrap(),
            None,
            "expected absent key to return None"
        );
    }
}

#[test]
fn test_empty_sst_round_trip() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("empty.sst");
    let opts = SstOptions::new();

    let meta = SstWriter::write(&sst_path, 7, vec![], &opts).unwrap();
    assert_eq!(
        meta,
        SstMetadata {
            id: 7,
            path: sst_path.clone(),
            entry_count: 0,
            min_key: None,
            max_key: None,
            min_version: None,
            max_version: None,
        }
    );

    let reader = SstReader::open(&sst_path).unwrap();
    assert_eq!(reader.metadata(), &meta);
    assert_eq!(reader.get(0, b"anything", Version::new(10)).unwrap(), None);

    let mut iter = reader.iter().unwrap();
    assert!(iter.next().is_none());
}

#[test]
fn test_single_entry_larger_than_block_bytes() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("large_entry.sst");
    // Block bytes small (128 bytes)
    let opts = SstOptions::new().with_block_bytes(128);

    let large_string = "x".repeat(1024);
    let entries = vec![
        put_entry(0, b"k1", 1, 1, "small"),
        put_entry(0, b"k2_large", 1, 2, &large_string),
        put_entry(0, b"k3", 1, 3, "small"),
    ];

    let meta = SstWriter::write(&sst_path, 8, entries.clone(), &opts).unwrap();
    assert_eq!(meta.entry_count, 3);

    let reader = SstReader::open(&sst_path).unwrap();
    let read_entries: Vec<MemtableEntry> = reader.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(read_entries, entries);

    let large_read = reader
        .get(0, b"k2_large", Version::new(1))
        .unwrap()
        .unwrap();
    assert_eq!(large_read, entries[1]);
}

#[test]
fn test_writer_rejects_unsorted_and_duplicate_keys() {
    let dir = tempdir().unwrap();
    let opts = SstOptions::new();

    // 1. Unsorted user keys
    let unsorted_keys = vec![
        put_entry(0, b"b", 1, 1, "val"),
        put_entry(0, b"a", 1, 2, "val"),
    ];
    let res = SstWriter::write(&dir.path().join("unsorted1.sst"), 9, unsorted_keys, &opts);
    assert!(res.is_err());

    // 2. Unsorted versions (version must be DESC)
    let unsorted_versions = vec![
        put_entry(0, b"a", 1, 1, "val"),
        put_entry(0, b"a", 2, 2, "val"),
    ];
    let res = SstWriter::write(
        &dir.path().join("unsorted2.sst"),
        10,
        unsorted_versions,
        &opts,
    );
    assert!(res.is_err());

    // 3. Duplicate key (exact same partition, user_key, and version)
    let duplicate = vec![
        put_entry(0, b"a", 1, 1, "val"),
        put_entry(0, b"a", 1, 1, "val"),
    ];
    let res = SstWriter::write(&dir.path().join("dup.sst"), 11, duplicate, &opts);
    assert!(res.is_err());
}

#[test]
fn test_corruption_byte_flipped_in_data_block() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("corrupt_block.sst");
    let opts = SstOptions::new().with_block_bytes(128);

    let mut entries = Vec::new();
    for i in 0..20 {
        let key = format!("k_{i:02}").into_bytes();
        entries.push(put_entry(0, &key, 1, i, "data"));
    }
    SstWriter::write(&sst_path, 12, entries, &opts).unwrap();

    // Flip a byte in the first block payload (header is 8 bytes, frame header is 8 bytes, so offset 18 is payload)
    flip_byte_at(&sst_path, 18);

    // Open reads the first block and should detect CRC mismatch
    let res = SstReader::open(&sst_path);
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_corruption_byte_flipped_in_middle_data_block() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("corrupt_mid_block.sst");
    let opts = SstOptions::new().with_block_bytes(100);

    let mut entries = Vec::new();
    for i in 0..30 {
        let key = format!("key_{i:02}").into_bytes();
        entries.push(put_entry(0, &key, 1, i, "payload_padding_data"));
    }
    SstWriter::write(&sst_path, 13, entries, &opts).unwrap();

    let reader_init = SstReader::open(&sst_path).unwrap();
    // Find an offset inside the second block payload
    let mid_block_offset = {
        let block2 = &reader_init.metadata();
        assert!(block2.entry_count > 0);
        // Let's read the file length and flip around 1/3 of the way into the file
        let len = std::fs::metadata(&sst_path).unwrap().len();
        len / 2
    };
    drop(reader_init);

    flip_byte_at(&sst_path, mid_block_offset);

    // Open might succeed if it only reads first and last block, but iter or get on mid block must fail with Corruption
    let res = SstReader::open(&sst_path).and_then(|r| r.iter()?.collect::<Result<Vec<_>, _>>());
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_corruption_byte_flipped_in_footer() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("corrupt_footer.sst");
    let opts = SstOptions::new();

    let entries = vec![put_entry(0, b"key1", 1, 1, "val1")];
    SstWriter::write(&sst_path, 14, entries, &opts).unwrap();

    let len = std::fs::metadata(&sst_path).unwrap().len();
    // Trailer is last 12 bytes; footer payload is right before trailer
    let footer_payload_offset = len - 16;
    flip_byte_at(&sst_path, footer_payload_offset);

    let res = SstReader::open(&sst_path);
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_corruption_file_truncated() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("truncated.sst");
    let opts = SstOptions::new();

    let entries = vec![put_entry(0, b"key1", 1, 1, "val1")];
    SstWriter::write(&sst_path, 15, entries, &opts).unwrap();

    let len = std::fs::metadata(&sst_path).unwrap().len();
    let f = OpenOptions::new().write(true).open(&sst_path).unwrap();
    f.set_len(len - 5).unwrap();
    f.sync_all().unwrap();
    drop(f);

    let res = SstReader::open(&sst_path);
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_corruption_trailing_magic_corrupted() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("corrupt_magic.sst");
    let opts = SstOptions::new();

    let entries = vec![put_entry(0, b"key1", 1, 1, "val1")];
    SstWriter::write(&sst_path, 16, entries, &opts).unwrap();

    let len = std::fs::metadata(&sst_path).unwrap().len();
    // Corrupt very last byte
    flip_byte_at(&sst_path, len - 1);

    let res = SstReader::open(&sst_path);
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_binary_keys() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("binary_keys.sst");
    let opts = SstOptions::new();

    let k_empty = b"".to_vec();
    let k_null = b"foo\x00bar".to_vec();
    let k_prefix = b"prefix".to_vec();
    let k_prefix_ext = b"prefix_more".to_vec();

    // Verify ordering
    assert!(k_empty < k_null);
    assert!(k_null < k_prefix);
    assert!(k_prefix < k_prefix_ext);

    let entries = vec![
        put_entry(0, &k_empty, 1, 0, "empty"),
        put_entry(0, &k_null, 1, 1, "null"),
        put_entry(0, &k_prefix, 1, 2, "prefix"),
        put_entry(0, &k_prefix_ext, 1, 3, "prefix_more"),
    ];

    SstWriter::write(&sst_path, 17, entries.clone(), &opts).unwrap();
    let reader = SstReader::open(&sst_path).unwrap();

    for entry in &entries {
        let found = reader
            .get(0, &entry.key.user_key, Version::new(1))
            .unwrap()
            .unwrap();
        assert_eq!(found, *entry);
    }

    let read_all: Vec<MemtableEntry> = reader.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(read_all, entries);
}

#[test]
fn test_multiple_partitions_in_one_sst() {
    let dir = tempdir().unwrap();
    let sst_path = dir.path().join("partitions.sst");
    let opts = SstOptions::new();

    let entries = vec![
        put_entry(0, b"k", 1, 1, "part0"),
        put_entry(1, b"k", 1, 2, "part1"),
        put_entry(5, b"a", 1, 3, "part5_a"),
        put_entry(5, b"b", 1, 4, "part5_b"),
        put_entry(100, b"z", 1, 5, "part100_z"),
    ];

    SstWriter::write(&sst_path, 18, entries.clone(), &opts).unwrap();
    let reader = SstReader::open(&sst_path).unwrap();

    for entry in &entries {
        let found = reader
            .get(entry.key.partition_id, &entry.key.user_key, Version::new(1))
            .unwrap()
            .unwrap();
        assert_eq!(found, *entry);
    }

    let read_all: Vec<MemtableEntry> = reader.iter().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(read_all, entries);
}

fn flip_byte_at(path: &Path, offset: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}
