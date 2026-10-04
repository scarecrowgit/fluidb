use std::path::Path;

use htap_colstore::{ScanRequest, SegmentOptions, SegmentReader, SegmentWriter};
use htap_common::fs::{create_dir_all_durable, sync_dir};
use htap_common::{ColumnDef, DataType, Row, Schema, Value};
use htap_crashsim::{CrashHarness, CrashInfo, CrashPolicy};

const ACK_LABEL: &str = "segment-written";

#[derive(Debug, PartialEq)]
enum ReadOutcome {
    Rows(Vec<Row>),
    Error(String),
}

fn schema() -> Schema {
    Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "name".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap()
}

fn rows() -> Vec<Row> {
    vec![
        Row::new(vec![Value::Int64(1), Value::String("alpha".into())]),
        Row::new(vec![Value::Int64(2), Value::String("beta".into())]),
        Row::new(vec![Value::Int64(3), Value::String("gamma".into())]),
        Row::new(vec![Value::Int64(4), Value::String("delta".into())]),
    ]
}

fn read_segment(path: &Path) -> ReadOutcome {
    let result: htap_common::Result<Vec<Row>> = (|| {
        let reader = SegmentReader::open(path)?;
        let result = reader.scan(&ScanRequest::new(vec![0, 1], None))?;

        Ok(result
            .batches
            .iter()
            .flat_map(|batch| {
                (0..batch.num_rows()).map(|row| {
                    Row::new(vec![
                        batch.columns[0]
                            .get(row)
                            .expect("id column value must exist"),
                        batch.columns[1]
                            .get(row)
                            .expect("name column value must exist"),
                    ])
                })
            })
            .collect::<Vec<_>>())
    })();

    match result {
        Ok(rows) => ReadOutcome::Rows(rows),
        Err(error) => ReadOutcome::Error(error.to_string()),
    }
}

fn acked(info: &CrashInfo) -> bool {
    info.acked_labels.iter().any(|label| label == ACK_LABEL)
}

#[test]
fn segment_torn_write_never_decodes_silently() {
    let harness = CrashHarness::new("segment_torn_write_never_decodes_silently").unwrap();

    harness
        .run_workload(|workload| {
            let segment_dir = workload.root().join("segments");
            let segment_path = segment_dir.join("data.col");
            create_dir_all_durable(&segment_dir).unwrap();

            htap_common::fs::write_new_tmp_file(&segment_path, &[], None).unwrap();
            sync_dir(&segment_dir).unwrap();

            SegmentWriter::write(&segment_path, &schema(), rows(), &SegmentOptions::new()).unwrap();

            workload.ack(ACK_LABEL);
        })
        .unwrap();

    let final_snapshot = harness.snapshot().unwrap();
    let final_segment_path = Path::new("segments").join("data.col");
    let final_segment_file_id = final_snapshot.names[&final_segment_path];
    let final_segment_len = final_snapshot.files[&final_segment_file_id].len() as u64;

    let mut checked_images = 0;
    let mut checked_acked_image = false;
    let mut torn_content_images = 0;

    let mut strict_check_image = |root: &Path, info: &CrashInfo| {
        checked_images += 1;
        let path = root.join("segments").join("data.col");

        let first = read_segment(&path);
        let second = read_segment(&path);
        assert_eq!(
            second, first,
            "repeated reads of the same crash image produced different outcomes"
        );

        if let ReadOutcome::Rows(actual) = &first {
            assert_eq!(
                actual,
                &rows(),
                "a torn or partial segment decoded without an error"
            );
        }

        if acked(info) {
            checked_acked_image = true;
            assert_eq!(
                first,
                ReadOutcome::Rows(rows()),
                "acknowledged segment did not read back exactly as written"
            );
        }
    };

    harness
        .enumerate(&CrashPolicy::Strict, &mut strict_check_image)
        .unwrap();

    let mut torn_check_image = |root: &Path, info: &CrashInfo| {
        checked_images += 1;
        let path = root.join("segments").join("data.col");

        let file_len = path
            .exists()
            .then(|| std::fs::metadata(&path).unwrap().len());
        let first = read_segment(&path);
        let second = read_segment(&path);
        assert_eq!(
            second, first,
            "repeated reads of the same crash image produced different outcomes"
        );

        match &first {
            ReadOutcome::Rows(actual) => {
                assert_eq!(
                    actual,
                    &rows(),
                    "a torn or partial segment decoded without an error"
                );
            }
            ReadOutcome::Error(_) if file_len.is_some_and(|len| len != 0) => {
                torn_content_images += 1;
            }
            ReadOutcome::Error(_) => {}
        }

        if acked(info) {
            checked_acked_image = true;
            assert_eq!(
                first,
                ReadOutcome::Rows(rows()),
                "acknowledged segment did not read back exactly as written"
            );
        }
    };

    for seed in 1..=8 {
        harness
            .enumerate(
                &CrashPolicy::Torn {
                    seed,
                    sector_size: 16,
                },
                &mut torn_check_image,
            )
            .unwrap();
    }

    assert!(checked_images > 1, "expected multiple crash images");
    assert!(
        checked_acked_image,
        "expected at least one acknowledged segment image"
    );
    assert!(
        torn_content_images > 0,
        "expected at least one genuinely rejected non-empty image from Torn policy runs; \
         checked {checked_images} images with final segment length {final_segment_len}"
    );
}

// Mutation-control witnesses checked by crates/htap-crashsim/tests/mutation_controls.rs.
htap_crashsim::crashsim_witness!(
    witness_colstore_segment_write_sync,
    site = "colstore:segment_write_sync",
    body = segment_torn_write_never_decodes_silently
);
htap_crashsim::crashsim_witness!(
    witness_sync_dir_sync,
    site = "sync_dir:sync",
    body = segment_torn_write_never_decodes_silently
);
htap_crashsim::crashsim_witness!(
    witness_create_dir_all_durable_parent_sync,
    site = "create_dir_all_durable:parent_sync",
    body = segment_torn_write_never_decodes_silently
);
htap_crashsim::crashsim_control!(
    control_file,
    skip = File,
    body = segment_torn_write_never_decodes_silently
);
htap_crashsim::crashsim_control!(
    control_dir,
    skip = Directory,
    body = segment_torn_write_never_decodes_silently
);
