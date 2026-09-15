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
    CatalogSnapshot, ColumnManifestRef, ConversionDescriptor, ConversionPhase,
    ListPartitionDefinition, NodeId, PartitionAlteration, PartitionDefinition, PartitionDescriptor,
    PartitionId, PartitionSource, PartitioningDescriptor, PartitioningMethod, RangeBound,
    RangePartitionDefinition, ReplicaDescriptor, ReplicaId, StorageDescriptor, StorageFormat,
    TableDescriptor, TableId, TableSelector, TabletDescriptor, TabletId, MAX_MANIFEST_ROWS,
    MAX_MANIFEST_SEGMENTS,
};
pub use store::CatalogStore;
