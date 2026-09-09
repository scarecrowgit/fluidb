//! Catalog storage interface and common operations.

use htap_common::Result;

use crate::model::{CatalogSnapshot, TableDescriptor, TableId};

/// Core trait for catalog persistence and atomic snapshot updates.
pub trait CatalogStore: Send + Sync {
    /// Load the current catalog snapshot, or `None` if the catalog is empty.
    fn load(&self) -> Result<Option<CatalogSnapshot>>;

    /// Atomically compare and update the catalog snapshot from `expected_generation` to `next`.
    ///
    /// # Semantics
    /// - `expected_generation` must match the current catalog generation (0 for empty store).
    /// - `next.generation` must be strictly greater than `expected_generation`.
    /// - `next` snapshot must pass semantic validation (`next.validate()`).
    /// - On generation mismatch, returns [`htap_common::HtapError::Conflict`].
    /// - On validation failure or invalid next generation, returns [`htap_common::HtapError::InvalidArgument`].
    /// - A failed CAS must not modify disk or alter store state.
    fn compare_and_set(&self, expected_generation: u64, next: CatalogSnapshot) -> Result<()>;

    /// Retrieve the current generation, returning 0 if the catalog is empty.
    fn current_generation(&self) -> Result<u64> {
        Ok(self.load()?.map(|s| s.generation).unwrap_or(0))
    }

    /// Retrieve a table descriptor by its ID, if it exists in the current snapshot.
    fn get_table(&self, id: TableId) -> Result<Option<TableDescriptor>> {
        Ok(self.load()?.and_then(|s| s.table(id).cloned()))
    }

    /// Retrieve a table descriptor by its name, if it exists in the current snapshot.
    fn get_table_by_name(&self, name: &str) -> Result<Option<TableDescriptor>> {
        Ok(self.load()?.and_then(|s| s.table_by_name(name).cloned()))
    }

    /// List all table descriptors in the current catalog snapshot.
    fn list_tables(&self) -> Result<Vec<TableDescriptor>> {
        Ok(self.load()?.map(|s| s.tables).unwrap_or_default())
    }
}
