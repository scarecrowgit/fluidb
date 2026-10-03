use std::path::Path;

use htap_catalog::TabletId;
use htap_colstore::{ScanRequest, SegmentReader};
use htap_common::fs::create_dir_all_durable;
use htap_common::{ColumnDef, DataType, HtapError, Row, Schema, Value, Version};
use htap_convert::{
    open, resolve_segment_path, write_atomic, write_segment, SegmentOptions, TabletColumnManifest,
};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};

const TABLET_ID: TabletId = TabletId::new(1);

fn schema() -> Schema {
    Schema::new(vec![ColumnDef {
        name: "id".into(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
    }])
    .unwrap()
}

fn rows(last: i64) -> Vec<Row> {
    (1..=last)
        .map(|id| Row::new(vec![Value::Int64(id)]))
        .collect()
}

fn acked(info: &CrashInfo, label: &str) -> bool {
    info.acked_labels.iter().any(|value| value == label)
}

fn open_twice(root: &Path) -> Option<TabletColumnManifest> {
    let first = match open(root, TABLET_ID) {
        Ok(manifest) => manifest,
        Err(HtapError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(HtapError::Corruption(error)) => {
            panic!("reopening conversion output returned corruption: {error}");
        }
        Err(error) => panic!("reopening conversion output failed: {error}"),
    };

    let second = open(root, TABLET_ID).unwrap();
    assert_eq!(
        second, first,
        "a second manifest open changed recovered conversion state"
    );
    Some(first)
}

fn manifest_with_segment(root: &Path, generation: u64, row_count: i64) -> TabletColumnManifest {
    let entry = write_segment(
        root,
        TABLET_ID,
        generation,
        "seg-0.col",
        &schema(),
        rows(row_count),
        &SegmentOptions::new(),
    )
    .unwrap();

    TabletColumnManifest::new(
        generation,
        TABLET_ID,
        schema(),
        Version::new(1),
        vec![entry],
    )
}

fn check_manifest_images(harness: &CrashHarness, ack_label: &str, expected_generation: u64) {
    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 67,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                let recovered = open_twice(&root.join("columnar"));

                if acked(info, ack_label) {
                    checked_with_ack = true;
                    let manifest = recovered.as_ref().expect("acknowledged manifest is absent");
                    assert_eq!(
                        manifest.generation, expected_generation,
                        "acknowledged conversion did not retain its manifest generation"
                    );
                }

                if let Some(manifest) = recovered.as_ref() {
                    assert!(
                        (1..=expected_generation).contains(&manifest.generation),
                        "recovered manifest generation is outside this test's serial history"
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(
            checked_with_ack,
            "expected an acknowledged conversion crash image"
        );
    }
}

#[test]
fn convert_manifest_old_or_new_and_segments_valid() {
    let harness = CrashHarness::new("convert_manifest_old_or_new_and_segments_valid").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("columnar");
            create_dir_all_durable(&root).unwrap();

            let first = manifest_with_segment(&root, 1, 2);
            write_atomic(&root, &first).unwrap();
            workload.ack("manifest-1");

            let second = manifest_with_segment(&root, 2, 3);
            write_atomic(&root, &second).unwrap();
            workload.ack("manifest-2");
        })
        .unwrap();

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 71,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                let recovered = open_twice(&root.join("columnar"));

                if let Some(manifest) = recovered.as_ref() {
                    assert!(
                        manifest.generation == 1 || manifest.generation == 2,
                        "recovered manifest is not old or new"
                    );
                    // `open` validates every registered segment through SegmentReader.
                    assert!(
                        !manifest.segments.is_empty(),
                        "published conversion manifest unexpectedly has no segment"
                    );
                }

                if acked(info, "manifest-2") {
                    checked_with_ack = true;
                    assert_eq!(
                        recovered
                            .expect("acknowledged new manifest is absent")
                            .generation,
                        2
                    );
                } else if acked(info, "manifest-1") {
                    checked_with_ack = true;
                    assert!(
                        recovered
                            .expect("acknowledged old manifest is absent")
                            .generation
                            >= 1
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(
            checked_with_ack,
            "expected an acknowledged manifest publication image"
        );
    }
}

#[test]
fn convert_resumes_after_incomplete_boundary() {
    let harness = CrashHarness::new("convert_resumes_after_incomplete_boundary").unwrap();
    let mut published_manifest = None;

    harness
        .run_workload(|workload| {
            let root = workload.root().join("columnar");
            create_dir_all_durable(&root).unwrap();

            let manifest = manifest_with_segment(&root, 1, 3);
            published_manifest = Some(manifest);
            workload.ack("segments-written");

            write_atomic(&root, published_manifest.as_ref().unwrap()).unwrap();
            workload.ack("conversion-complete");
        })
        .unwrap();

    let published_manifest = published_manifest.expect("workload did not create a manifest");

    for policy in [
        CrashPolicy::Strict,
        CrashPolicy::Torn {
            seed: 67,
            sector_size: 4096,
        },
    ] {
        let mut checked_count = 0;
        let mut checked_with_ack = false;
        let mut resumed_images = 0;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;

                let columnar_root = root.join("columnar");
                let recovered = open_twice(&columnar_root);
                if acked(info, "segments-written") && !acked(info, "conversion-complete") {
                    let segment_path =
                        resolve_segment_path(&columnar_root, TABLET_ID, "gen-1/seg-0.col").unwrap();
                    assert!(
                        segment_path.exists(),
                        "segment written before the boundary is absent"
                    );
                    SegmentReader::open(&segment_path)
                        .expect("segment written before the boundary is unreadable");

                    write_atomic(&columnar_root, &published_manifest).unwrap();
                    let resumed = open(&columnar_root, TABLET_ID)
                        .expect("resumed conversion output failed to open");
                    assert_eq!(
                        &resumed, &published_manifest,
                        "resumed conversion output differs from the published manifest"
                    );
                    resumed_images += 1;
                }

                if let Some(manifest) = recovered.as_ref() {
                    assert_eq!(
                        manifest.generation, 1,
                        "recovered conversion manifest generation is outside this test's history"
                    );
                }

                if acked(info, "conversion-complete") {
                    checked_with_ack = true;
                    assert_eq!(
                        recovered
                            .expect("acknowledged conversion output is absent")
                            .generation,
                        1
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(
            checked_with_ack,
            "expected an acknowledged conversion completion image"
        );
        assert!(
            resumed_images > 0,
            "expected a crash image after segments were written but before manifest publication"
        );
    }
}

#[test]
fn convert_preexisting_volatile_root() {
    let harness = CrashHarness::new("convert_preexisting_volatile_root").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("columnar");
            htap_common::fs::dur::create_dir_all(&root).unwrap();

            let manifest = manifest_with_segment(&root, 1, 2);
            write_atomic(&root, &manifest).unwrap();
            workload.ack("manifest-published");
        })
        .unwrap();

    check_manifest_images(&harness, "manifest-published", 1);
}

#[test]
fn convert_fresh_dir_durable() {
    let harness = CrashHarness::new("convert_fresh_dir_durable").unwrap();

    harness
        .run_workload(|workload| {
            let root = workload.root().join("columnar");

            // `write_segment` and `write_atomic` create the tablet hierarchy themselves.
            let manifest = manifest_with_segment(&root, 1, 2);
            write_atomic(&root, &manifest).unwrap();
            workload.ack("manifest-published");
        })
        .unwrap();

    check_manifest_images(&harness, "manifest-published", 1);
}

#[test]
fn segment_published_name_never_torn() {
    let harness = CrashHarness::new("segment_published_name_never_torn").unwrap();
    let expected_rows = rows(3);

    harness
        .run_workload(|workload| {
            let root = workload.root().join("columnar");
            create_dir_all_durable(&root).unwrap();

            write_segment(
                &root,
                TABLET_ID,
                1,
                "seg-0.col",
                &schema(),
                expected_rows.clone(),
                &SegmentOptions::new(),
            )
            .unwrap();
            workload.ack("segment-published");
        })
        .unwrap();

    let policies =
        std::iter::once(CrashPolicy::Strict).chain((1..=8).map(|seed| CrashPolicy::Torn {
            seed,
            sector_size: 16,
        }));

    for policy in policies {
        let mut checked_count = 0;
        let mut checked_with_ack = false;

        harness
            .enumerate(&policy, |root, info| {
                checked_count += 1;
                let columnar_root = root.join("columnar");
                let final_path =
                    resolve_segment_path(&columnar_root, TABLET_ID, "gen-1/seg-0.col").unwrap();

                let read_final = || -> Option<Vec<Row>> {
                    if !final_path.exists() {
                        return None;
                    }

                    let reader = SegmentReader::open(&final_path)
                        .expect("final segment name exposed an unreadable segment");
                    let request = ScanRequest::new((0..schema().len()).collect(), None);
                    let result = reader
                        .scan(&request)
                        .expect("final segment name exposed an unscannable segment");
                    let mut recovered_rows = Vec::new();

                    for batch in result.batches {
                        for row_index in 0..batch.num_rows() {
                            let values = batch
                                .columns
                                .iter()
                                .map(|column| {
                                    column.get(row_index).expect(
                                        "final segment contains a missing row-aligned value",
                                    )
                                })
                                .collect();
                            recovered_rows.push(Row::new(values));
                        }
                    }

                    Some(recovered_rows)
                };

                let first = read_final();
                let second = read_final();
                assert_eq!(
                    second, first,
                    "reading the final segment name twice changed the recovered outcome"
                );

                if let Some(recovered_rows) = first.as_ref() {
                    assert_eq!(
                        recovered_rows, &expected_rows,
                        "final segment name exposed rows other than the complete written set"
                    );
                }

                if acked(info, "segment-published") {
                    checked_with_ack = true;
                    assert_eq!(
                        first.as_ref(),
                        Some(&expected_rows),
                        "acknowledged segment publication did not retain the complete final file"
                    );
                }
            })
            .unwrap();

        assert!(checked_count > 1, "expected multiple crash images");
        assert!(
            checked_with_ack,
            "expected an acknowledged segment publication image"
        );
    }
}
