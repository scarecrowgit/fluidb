//! Schema, tables, partitions, tablets, and metadata catalog.
//!
//! Provides a validated, serde-persistable catalog model and crash-safe
//! local repository for later conversion, SQL, distribution, and coordination.
#![forbid(unsafe_code)]

pub mod local;
pub mod model;
pub mod store;

pub use local::LocalCatalogStore;
pub use model::{
    Account, AccountId, CatalogSnapshot, ColumnManifestRef, ConversionDescriptor, ConversionPhase,
    Grant, IdHighWater, ListPartitionDefinition, NodeId, PartitionAlteration, PartitionDefinition,
    PartitionDescriptor, PartitionId, PartitionSource, PartitioningDescriptor, PartitioningMethod,
    PrivilegeScope, PrivilegeSet, RangeBound, RangePartitionDefinition, ReplicaDescriptor,
    ReplicaId, StorageDescriptor, StorageFormat, TableDescriptor, TableId, TableSelector,
    TabletDescriptor, TabletId, MAX_MANIFEST_ROWS, MAX_MANIFEST_SEGMENTS,
};
pub use store::CatalogStore;
