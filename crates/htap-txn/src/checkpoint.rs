//! Durable high-water checkpoint for compacted transaction journals.

use std::io::ErrorKind;
use std::path::Path;

use htap_common::envelope::{decode_envelope, encode_envelope, SizeCheckMode};
use htap_common::fs::{atomic_publish, read_file_exact_bounded};
use htap_common::{HtapError, Result, Version};
use serde::{Deserialize, Serialize};

const CHECKPOINT_MAGIC: &[u8; 8] = b"HTAPTXC1";
const CHECKPOINT_FORMAT_VERSION: u16 = 1;
const CHECKPOINT_MAX_PAYLOAD_BYTES: u32 = 1024;
const CHECKPOINT_FILE: &str = "txn.checkpoint";
const CHECKPOINT_TMP_FILE: &str = "txn.checkpoint.tmp";

/// High-water marks retained after journal compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointBaseline {
    /// Greatest allocated transaction identifier.
    pub txn_id_high_water: u64,
    /// Greatest committed MVCC version.
    pub version_high_water: Version,
}

impl Default for CheckpointBaseline {
    fn default() -> Self {
        Self {
            txn_id_high_water: 0,
            version_high_water: Version::INITIAL,
        }
    }
}

fn checkpoint_dir(root: &Path) -> Result<&Path> {
    root.parent().ok_or_else(|| {
        HtapError::Internal(format!(
            "journal path '{}' has no parent directory",
            root.display()
        ))
    })
}

/// Load the durable checkpoint adjacent to `root`.
pub fn load_checkpoint(root: &Path) -> Result<Option<CheckpointBaseline>> {
    let dir = checkpoint_dir(root)?;
    let path = dir.join(CHECKPOINT_FILE);
    let bytes = match read_file_exact_bounded(&path, 18 + CHECKPOINT_MAX_PAYLOAD_BYTES as usize) {
        Ok(bytes) => bytes,
        Err(HtapError::Io(error)) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    let (_, payload) = decode_envelope(
        &bytes,
        CHECKPOINT_MAGIC,
        CHECKPOINT_FORMAT_VERSION..=CHECKPOINT_FORMAT_VERSION,
        CHECKPOINT_MAX_PAYLOAD_BYTES,
        SizeCheckMode::TruncatedThenTrailing,
    )
    .map_err(|error| HtapError::Corruption(format!("invalid transaction checkpoint: {error:?}")))?;

    serde_json::from_slice(payload).map(Some).map_err(|error| {
        HtapError::Corruption(format!("invalid transaction checkpoint payload: {error}"))
    })
}

/// Atomically publish a checkpoint, refusing to regress an existing one.
pub fn publish_checkpoint(root: &Path, baseline: &CheckpointBaseline) -> Result<()> {
    if let Some(current) = load_checkpoint(root)? {
        if baseline.txn_id_high_water < current.txn_id_high_water
            || baseline.version_high_water < current.version_high_water
        {
            return Err(HtapError::Internal(
                "refusing to publish regressing transaction checkpoint".to_string(),
            ));
        }
    }

    let payload = serde_json::to_vec(baseline).map_err(|error| {
        HtapError::Internal(format!(
            "failed to serialize transaction checkpoint: {error}"
        ))
    })?;
    if payload.len() > CHECKPOINT_MAX_PAYLOAD_BYTES as usize {
        return Err(HtapError::Internal(
            "transaction checkpoint payload exceeds its bounded format size".to_string(),
        ));
    }

    atomic_publish(
        checkpoint_dir(root)?,
        CHECKPOINT_TMP_FILE,
        CHECKPOINT_FILE,
        &encode_envelope(CHECKPOINT_MAGIC, CHECKPOINT_FORMAT_VERSION, &payload),
        None,
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_root(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "htap-txn-checkpoint-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("journal")
    }

    fn cleanup(root: &Path) {
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    fn baseline() -> CheckpointBaseline {
        CheckpointBaseline {
            txn_id_high_water: 42,
            version_high_water: Version::new(7),
        }
    }

    fn checkpoint_path(root: &Path) -> PathBuf {
        root.parent().unwrap().join(CHECKPOINT_FILE)
    }

    #[test]
    fn checkpoint_round_trips() {
        let root = test_root("round-trip");
        let expected = baseline();

        publish_checkpoint(&root, &expected).unwrap();

        assert_eq!(load_checkpoint(&root).unwrap(), Some(expected));
        cleanup(&root);
    }

    #[test]
    fn absent_checkpoint_returns_none() {
        let root = test_root("absent");

        assert_eq!(load_checkpoint(&root).unwrap(), None);

        cleanup(&root);
    }

    #[test]
    fn corrupted_checkpoint_crc_is_rejected() {
        let root = test_root("bad-crc");
        let mut bytes = encode_envelope(
            CHECKPOINT_MAGIC,
            CHECKPOINT_FORMAT_VERSION,
            &serde_json::to_vec(&baseline()).unwrap(),
        );
        bytes[14] ^= 0xff;
        fs::write(checkpoint_path(&root), bytes).unwrap();

        assert!(matches!(
            load_checkpoint(&root),
            Err(HtapError::Corruption(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn future_checkpoint_version_is_rejected() {
        let root = test_root("future-version");
        let mut bytes = encode_envelope(
            CHECKPOINT_MAGIC,
            CHECKPOINT_FORMAT_VERSION,
            &serde_json::to_vec(&baseline()).unwrap(),
        );
        bytes[8..10].copy_from_slice(&(CHECKPOINT_FORMAT_VERSION + 1).to_le_bytes());
        fs::write(checkpoint_path(&root), bytes).unwrap();

        assert!(matches!(
            load_checkpoint(&root),
            Err(HtapError::Corruption(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn truncated_checkpoint_is_rejected() {
        let root = test_root("truncated");
        let mut bytes = encode_envelope(
            CHECKPOINT_MAGIC,
            CHECKPOINT_FORMAT_VERSION,
            &serde_json::to_vec(&baseline()).unwrap(),
        );
        bytes.pop();
        fs::write(checkpoint_path(&root), bytes).unwrap();

        assert!(matches!(
            load_checkpoint(&root),
            Err(HtapError::Corruption(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn checkpoint_with_trailing_bytes_is_rejected() {
        let root = test_root("trailing");
        let mut bytes = encode_envelope(
            CHECKPOINT_MAGIC,
            CHECKPOINT_FORMAT_VERSION,
            &serde_json::to_vec(&baseline()).unwrap(),
        );
        bytes.push(0xaa);
        fs::write(checkpoint_path(&root), bytes).unwrap();

        assert!(matches!(
            load_checkpoint(&root),
            Err(HtapError::Corruption(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn publish_refuses_regressing_txn_id_high_water() {
        let root = test_root("regressing-txn-id");
        publish_checkpoint(&root, &baseline()).unwrap();

        let regressing = CheckpointBaseline {
            txn_id_high_water: 41,
            version_high_water: Version::new(7),
        };
        assert!(matches!(
            publish_checkpoint(&root, &regressing),
            Err(HtapError::Internal(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn publish_refuses_regressing_version_high_water() {
        let root = test_root("regressing-version");
        publish_checkpoint(&root, &baseline()).unwrap();

        let regressing = CheckpointBaseline {
            txn_id_high_water: 42,
            version_high_water: Version::new(6),
        };
        assert!(matches!(
            publish_checkpoint(&root, &regressing),
            Err(HtapError::Internal(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn checkpoint_envelope_matches_golden_bytes() {
        // Golden bytes captured for txn_id_high_water=42 and version_high_water=7.
        const GOLDEN_PREFIX: &[u8] = b"HTAPTXC1\x01\x00\x2f\x00\x00\x00";
        const GOLDEN_PAYLOAD: &[u8] = br#"{"txn_id_high_water":42,"version_high_water":7}"#;

        let expected = CheckpointBaseline {
            txn_id_high_water: 42,
            version_high_water: Version::new(7),
        };
        let serialized_json = serde_json::to_vec(&expected).unwrap();
        let encoded = encode_envelope(
            CHECKPOINT_MAGIC,
            CHECKPOINT_FORMAT_VERSION,
            &serialized_json,
        );

        assert_eq!(&encoded[..14], GOLDEN_PREFIX);
        assert_eq!(&encoded[18..], GOLDEN_PAYLOAD);
        assert_eq!(
            serde_json::from_slice::<CheckpointBaseline>(&encoded[18..]).unwrap(),
            expected
        );
    }
}
