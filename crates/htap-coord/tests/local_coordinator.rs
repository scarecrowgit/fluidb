//! Integration tests for [`LocalCoordinator`].

use std::fs;

use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, LocalCatalogStore, NodeId, PartitionDescriptor, PartitionId,
    ReplicaDescriptor, ReplicaId, StorageDescriptor, TableDescriptor, TableId, TabletDescriptor,
    TabletId,
};
use htap_common::{ColumnDef, DataType, HtapError, Schema};
use htap_coord::{
    Coordinator, LocalCoordinator, COORDINATOR_FILE_NAME, FORMAT_VERSION, HEADER_LEN, HEADER_MAGIC,
};
use tempfile::TempDir;

fn make_schema() -> Schema {
    Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".to_string(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap()
}

fn make_valid_snapshot(generation: u64) -> CatalogSnapshot {
    let schema = make_schema();
    let table = TableDescriptor::new(
        TableId::new(1),
        "users",
        schema,
        vec![0],
        vec![PartitionId::new(10)],
        generation,
    );
    let partition = PartitionDescriptor::new(
        PartitionId::new(10),
        TableId::new(1),
        "p0",
        StorageDescriptor::Row,
        vec![TabletId::new(100)],
        generation,
    );
    let tablet = TabletDescriptor::new(
        TabletId::new(100),
        PartitionId::new(10),
        0,
        vec![ReplicaId::new(1000)],
        generation,
    );
    let replica = ReplicaDescriptor::new(
        ReplicaId::new(1000),
        TabletId::new(100),
        NodeId::new(42),
        true,
        true,
        generation,
    );

    CatalogSnapshot::new(
        generation,
        vec![table],
        vec![partition],
        vec![tablet],
        vec![replica],
    )
}

#[test]
fn test_empty_and_reopen_state() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("coord");

    {
        let coord = LocalCoordinator::open(&root).unwrap();
        assert!(coord.list_nodes().unwrap().is_empty());
        assert!(coord.current_leadership("any_scope").unwrap().is_none());
    }

    // Reopen and verify still empty and valid
    {
        let coord = LocalCoordinator::open(&root).unwrap();
        assert!(coord.list_nodes().unwrap().is_empty());
        assert!(coord.current_leadership("any_scope").unwrap().is_none());
    }
}

#[test]
fn test_member_ordering() {
    let tmp = TempDir::new().unwrap();
    let coord = LocalCoordinator::open(tmp.path()).unwrap();

    // Register out of order
    coord.register_node(NodeId::new(42)).unwrap();
    coord.register_node(NodeId::new(7)).unwrap();
    coord.register_node(NodeId::new(100)).unwrap();
    coord.register_node(NodeId::new(1)).unwrap();

    // Check deterministic ascending order
    let expected = vec![
        NodeId::new(1),
        NodeId::new(7),
        NodeId::new(42),
        NodeId::new(100),
    ];
    assert_eq!(coord.list_nodes().unwrap(), expected);

    // Idempotent re-registration
    coord.register_node(NodeId::new(7)).unwrap();
    assert_eq!(coord.list_nodes().unwrap(), expected);

    // Remove node
    coord.remove_node(NodeId::new(7)).unwrap();
    let expected_after_removal = vec![NodeId::new(1), NodeId::new(42), NodeId::new(100)];
    assert_eq!(coord.list_nodes().unwrap(), expected_after_removal);

    // Idempotent non-existent removal
    coord.remove_node(NodeId::new(999)).unwrap();
    assert_eq!(coord.list_nodes().unwrap(), expected_after_removal);

    drop(coord);

    // Reopen preserves deterministic member order
    let reopened = LocalCoordinator::open(tmp.path()).unwrap();
    assert_eq!(reopened.list_nodes().unwrap(), expected_after_removal);
}

#[test]
fn test_token_monotonicity_and_reopen_no_reuse() {
    let tmp = TempDir::new().unwrap();

    let (t1, t2, t3) = {
        let coord = LocalCoordinator::open(tmp.path()).unwrap();

        let l1 = coord
            .acquire_leadership("scope_a", NodeId::new(10))
            .unwrap();
        let l2 = coord
            .acquire_leadership("scope_b", NodeId::new(20))
            .unwrap();
        assert!(l2.token > l1.token);

        let l3 = coord
            .replace_leadership("scope_a", NodeId::new(11))
            .unwrap();
        assert!(l3.token > l2.token);

        (l1.token, l2.token, l3.token)
    };

    // Reopen coordinator and ensure tokens are strictly monotonically increasing
    // and never reused.
    {
        let reopened = LocalCoordinator::open(tmp.path()).unwrap();

        // Scope leadership should be preserved
        let cur_a = reopened.current_leadership("scope_a").unwrap().unwrap();
        assert_eq!(cur_a.holder, NodeId::new(11));
        assert_eq!(cur_a.token, t3);

        let cur_b = reopened.current_leadership("scope_b").unwrap().unwrap();
        assert_eq!(cur_b.holder, NodeId::new(20));
        assert_eq!(cur_b.token, t2);

        // New acquisition after reopen must strictly exceed all previous tokens
        let l4 = reopened
            .acquire_leadership("scope_c", NodeId::new(30))
            .unwrap();
        assert!(l4.token > t3);
        assert!(l4.token > t2);
        assert!(l4.token > t1);
    }
}

#[test]
fn test_stale_fence_rejection() {
    let tmp = TempDir::new().unwrap();
    let coord = LocalCoordinator::open(tmp.path()).unwrap();

    let l1 = coord
        .acquire_leadership("scope_fence", NodeId::new(1))
        .unwrap();
    assert!(coord.validate_fence("scope_fence", l1.token).is_ok());

    let l2 = coord
        .replace_leadership("scope_fence", NodeId::new(2))
        .unwrap();
    assert!(l2.token > l1.token);

    // Stale token must be rejected with HtapError::Fenced { expected, got }
    let err = coord.validate_fence("scope_fence", l1.token).unwrap_err();
    match err {
        HtapError::Fenced { expected, got } => {
            assert_eq!(expected, l2.token.get());
            assert_eq!(got, l1.token.get());
        }
        other => panic!("expected HtapError::Fenced, got {other:?}"),
    }

    // Current token is accepted
    assert!(coord.validate_fence("scope_fence", l2.token).is_ok());

    // Non-existent scope returns NotFound
    let err_nf = coord.validate_fence("unknown_scope", l2.token).unwrap_err();
    assert!(matches!(err_nf, HtapError::NotFound(_)));
}

#[test]
fn test_fenced_catalog_cas_stale_and_current() {
    let tmp_cat = TempDir::new().unwrap();
    let tmp_coord = TempDir::new().unwrap();

    let catalog = LocalCatalogStore::open(tmp_cat.path()).unwrap();
    let coord = LocalCoordinator::open(tmp_coord.path()).unwrap();

    // Initialize catalog at generation 1
    let snap1 = make_valid_snapshot(1);
    catalog.compare_and_set(0, snap1).unwrap();
    assert_eq!(catalog.current_generation().unwrap(), 1);

    // Leader 1 acquires scope
    let l1 = coord
        .acquire_leadership("catalog_scope", NodeId::new(1))
        .unwrap();

    // Leader 2 replaces scope
    let l2 = coord
        .replace_leadership("catalog_scope", NodeId::new(2))
        .unwrap();
    assert!(l2.token > l1.token);

    let snap2 = make_valid_snapshot(2);

    // Leader 1 attempts CAS with stale token -> rejected, catalog unchanged!
    let err = coord
        .fenced_catalog_compare_and_set("catalog_scope", l1.token, &catalog, 1, snap2.clone())
        .unwrap_err();

    match err {
        HtapError::Fenced { expected, got } => {
            assert_eq!(expected, l2.token.get());
            assert_eq!(got, l1.token.get());
        }
        other => panic!("expected HtapError::Fenced, got {other:?}"),
    }
    // Verify catalog was NOT updated
    assert_eq!(catalog.current_generation().unwrap(), 1);

    // Leader 2 attempts CAS with current token -> succeeds!
    coord
        .fenced_catalog_compare_and_set("catalog_scope", l2.token, &catalog, 1, snap2)
        .unwrap();
    assert_eq!(catalog.current_generation().unwrap(), 2);

    // Leader 2 attempts CAS with mismatched catalog generation -> Conflict from catalog
    let snap3 = make_valid_snapshot(3);
    let err_conflict = coord
        .fenced_catalog_compare_and_set(
            "catalog_scope",
            l2.token,
            &catalog,
            999, // wrong expected generation
            snap3,
        )
        .unwrap_err();
    assert!(matches!(err_conflict, HtapError::Conflict(_)));
}

#[test]
fn test_release_and_replacement() {
    let tmp = TempDir::new().unwrap();
    let coord = LocalCoordinator::open(tmp.path()).unwrap();

    let l1 = coord
        .acquire_leadership("lease_scope", NodeId::new(1))
        .unwrap();

    // Conflict when attempting to acquire already-held scope
    let conflict = coord
        .acquire_leadership("lease_scope", NodeId::new(2))
        .unwrap_err();
    assert!(matches!(conflict, HtapError::Conflict(_)));

    // Release leadership
    coord.release_leadership("lease_scope").unwrap();
    assert!(coord.current_leadership("lease_scope").unwrap().is_none());

    // Fence validation for released scope fails with Fenced
    let err_fenced = coord.validate_fence("lease_scope", l1.token).unwrap_err();
    match err_fenced {
        HtapError::Fenced { expected, got } => {
            assert!(expected > l1.token.get());
            assert_eq!(got, l1.token.get());
        }
        other => panic!("expected HtapError::Fenced, got {other:?}"),
    }

    // Reacquire after release issues strictly increasing token
    let l2 = coord
        .acquire_leadership("lease_scope", NodeId::new(2))
        .unwrap();
    assert!(l2.token > l1.token);
    assert_eq!(l2.holder, NodeId::new(2));

    // Replace leadership issues strictly increasing token
    let l3 = coord
        .replace_leadership("lease_scope", NodeId::new(3))
        .unwrap();
    assert!(l3.token > l2.token);
    assert_eq!(l3.holder, NodeId::new(3));
    assert_eq!(
        coord
            .current_leadership("lease_scope")
            .unwrap()
            .unwrap()
            .holder,
        NodeId::new(3)
    );

    // Release unheld scope succeeds idempotently
    coord.release_leadership("never_held_scope").unwrap();
}

#[test]
fn test_malformed_envelope() {
    let tmp = TempDir::new().unwrap();
    let coord_file = tmp.path().join(COORDINATOR_FILE_NAME);

    // 1. File too small (< HEADER_LEN)
    fs::write(&coord_file, b"short").unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 2. Bad magic bytes
    let mut bad_magic = vec![0u8; HEADER_LEN + 10];
    bad_magic[0..8].copy_from_slice(b"BADMAGIC");
    fs::write(&coord_file, &bad_magic).unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 3. Unsupported format version
    let mut bad_ver = vec![0u8; HEADER_LEN + 10];
    bad_ver[0..8].copy_from_slice(HEADER_MAGIC);
    bad_ver[8..10].copy_from_slice(&99u16.to_le_bytes());
    fs::write(&coord_file, &bad_ver).unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 4. Truncated payload
    let mut truncated = vec![0u8; HEADER_LEN + 5];
    truncated[0..8].copy_from_slice(HEADER_MAGIC);
    truncated[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    truncated[10..14].copy_from_slice(&100u32.to_le_bytes()); // claims 100 bytes payload
    fs::write(&coord_file, &truncated).unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 5. Bad CRC32C checksum
    let payload = b"{\"members\":[],\"leaders\":{},\"scope_tokens\":{},\"next_token\":1}";
    let payload_len = payload.len() as u32;
    let bad_crc = 0xdeadbeef_u32;
    let mut bad_crc_file = Vec::new();
    bad_crc_file.extend_from_slice(HEADER_MAGIC);
    bad_crc_file.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bad_crc_file.extend_from_slice(&payload_len.to_le_bytes());
    bad_crc_file.extend_from_slice(&bad_crc.to_le_bytes());
    bad_crc_file.extend_from_slice(payload);
    fs::write(&coord_file, &bad_crc_file).unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 6. Trailing garbage bytes
    let real_crc = crc32c::crc32c(payload);
    let mut trailing_file = Vec::new();
    trailing_file.extend_from_slice(HEADER_MAGIC);
    trailing_file.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    trailing_file.extend_from_slice(&payload_len.to_le_bytes());
    trailing_file.extend_from_slice(&real_crc.to_le_bytes());
    trailing_file.extend_from_slice(payload);
    trailing_file.extend_from_slice(b"garbage_at_end");
    fs::write(&coord_file, &trailing_file).unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 7. Invalid JSON payload
    let bad_json_payload = b"this is definitely not valid json!!";
    let bad_json_len = bad_json_payload.len() as u32;
    let bad_json_crc = crc32c::crc32c(bad_json_payload);
    let mut bad_json_file = Vec::new();
    bad_json_file.extend_from_slice(HEADER_MAGIC);
    bad_json_file.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bad_json_file.extend_from_slice(&bad_json_len.to_le_bytes());
    bad_json_file.extend_from_slice(&bad_json_crc.to_le_bytes());
    bad_json_file.extend_from_slice(bad_json_payload);
    fs::write(&coord_file, &bad_json_file).unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
}

#[test]
fn test_coordinator_object_safety() {
    let tmp = TempDir::new().unwrap();
    let coord = LocalCoordinator::open(tmp.path()).unwrap();

    let boxed: Box<dyn Coordinator> = Box::new(coord);
    boxed.register_node(NodeId::new(1)).unwrap();
    let nodes = boxed.list_nodes().unwrap();
    assert_eq!(nodes, vec![NodeId::new(1)]);

    let leadership = boxed
        .acquire_leadership("boxed_scope", NodeId::new(1))
        .unwrap();
    assert_eq!(leadership.scope, "boxed_scope");
    boxed
        .validate_fence("boxed_scope", leadership.token)
        .unwrap();

    // Verify alias methods work via trait object
    boxed.register_member(NodeId::new(2)).unwrap();
    let members = boxed.list_members().unwrap();
    assert_eq!(members, vec![NodeId::new(1), NodeId::new(2)]);

    let cur = boxed.current("boxed_scope").unwrap().unwrap();
    assert_eq!(cur.holder, NodeId::new(1));

    boxed.release("boxed_scope").unwrap();
    assert!(boxed.current("boxed_scope").unwrap().is_none());

    let replaced = boxed.replace("boxed_scope", NodeId::new(2)).unwrap();
    assert_eq!(replaced.holder, NodeId::new(2));

    boxed.remove_member(NodeId::new(1)).unwrap();
    assert_eq!(boxed.list_members().unwrap(), vec![NodeId::new(2)]);
}

fn coord_child_binary() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("test executable path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop(); // .../target/<profile>
    }
    let exe = format!("coord_lock_child{}", std::env::consts::EXE_SUFFIX);
    let candidate = dir.join(&exe);
    if !candidate.is_file() {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "htap-coord", "--bin", "coord_lock_child"])
            .status()
            .expect("building coord_lock_child");
        assert!(status.success(), "cargo build coord_lock_child failed");
    }
    assert!(
        candidate.is_file(),
        "coord_lock_child binary not found at {}",
        candidate.display()
    );
    candidate
}

#[test]
fn test_subprocess_exclusive_lock_contention_and_symlink() {
    use std::io::BufRead;

    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // 1. Spawn child holding lock
    let mut child = std::process::Command::new(coord_child_binary())
        .arg(root)
        .arg("hold")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn coord_lock_child");

    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read from child");
    assert_eq!(line.trim(), "LOCKED");

    // 2. Child is holding lock. Opening from parent process must fail with Conflict.
    let err = LocalCoordinator::open(root).unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict error, got {err:?}"
    );
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("exclusive root lock contention"),
        "error message did not mention contention: {err_msg}"
    );
    assert!(
        err_msg.contains(&format!("pid={}", child.id())),
        "error message should contain child pid: {err_msg}"
    );

    // 3. Symlink alias test where supported: opening via symlink must also fail with Conflict
    #[cfg(unix)]
    {
        let symlink_parent = TempDir::new().unwrap();
        let symlink_path = symlink_parent.path().join("coord_symlink_alias");
        std::os::unix::fs::symlink(root, &symlink_path).unwrap();

        let sym_err = LocalCoordinator::open(&symlink_path).unwrap_err();
        assert!(
            matches!(sym_err, HtapError::Conflict(_)),
            "expected Conflict on symlink open, got {sym_err:?}"
        );
        let sym_err_msg = sym_err.to_string();
        assert!(
            sym_err_msg.contains("exclusive root lock contention"),
            "symlink error message: {sym_err_msg}"
        );
    }

    // 4. Second child opening same root must also fail with Conflict (exit code 42)
    let child2_output = std::process::Command::new(coord_child_binary())
        .arg(root)
        .arg("try_once")
        .output()
        .expect("spawn second child");
    assert_eq!(
        child2_output.status.code(),
        Some(42),
        "second child should exit with Conflict status code 42"
    );

    // 5. Release child lock by dropping its stdin and waiting for exit
    drop(child.stdin.take());
    let status = child.wait().expect("wait on child");
    assert!(status.success(), "child did not exit cleanly: {status:?}");

    // 6. After child exits, reopen succeeds and operates normally
    let coord = LocalCoordinator::open(root).expect("reopen after child exit should succeed");
    coord
        .register_node(NodeId::new(42))
        .expect("register should succeed");
    assert_eq!(coord.list_nodes().unwrap(), vec![NodeId::new(42)]);
}

#[test]
fn test_coordinator_bounds_oversized_and_short() {
    use htap_coord::{COORDINATOR_FILE_NAME, HEADER_LEN, MAX_COORDINATOR_PAYLOAD_BYTES};

    let tmp = TempDir::new().unwrap();
    let coord_path = tmp.path().join(COORDINATOR_FILE_NAME);

    // Oversized physical file rejected before allocation
    let file = fs::File::create(&coord_path).unwrap();
    let oversized_len = (HEADER_LEN as u64) + (MAX_COORDINATOR_PAYLOAD_BYTES as u64) + 1;
    file.set_len(oversized_len).unwrap();
    drop(file);

    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("exceeds maximum allowed bound"));

    // Short header file (< HEADER_LEN) rejected as corruption
    fs::write(&coord_path, b"SHORT").unwrap();
    let err = LocalCoordinator::open(tmp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
}
