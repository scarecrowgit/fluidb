//! SQL route classification.
//!
//! Classifies catalog-bound statements into execution routes based on the target
//! partition's storage format descriptor.
//!
//! # Authoritative Rowstore Architecture
//!
//! The LSM rowstore is authoritative for transactional writes and point reads across
//! all partition storage formats: during and after local conversion ([`StorageDescriptor::Row`],
//! [`StorageDescriptor::Column`], and [`StorageDescriptor::Converting`]). Columnar storage
//! provides an analytical acceleration format populated asynchronously or via conversion,
//! while the transactional engine continues to serve point queries and mutations directly
//! from the authoritative rowstore.
//!
//! # Structural R5 Guarantee
//!
//! In accordance with the R5 architectural principle (mixed OLTP/OLAP workload separation),
//! this route classifier contains no analytical or columnar storage execution dependencies.
//! Point lookups and rowstore mutations are classified directly for the transactional engine
//! without incurring any analytical planning, physical optimization, or vectorized execution
//! overhead, avoiding any dependency on `htap-colstore`.
//!
//! Table scans and other non-point operations remain unsupported on this path.
//!
//! # Partition Selection
//!
//! Partition selection is intentionally outside this pure classifier because the current
//! catalog does not define a SQL partition-selection rule (such as range or hash partitioning
//! expressions on table columns). Partition routing at this stage is determined by the
//! resolved partition's [`StorageDescriptor`].

use htap_catalog::StorageDescriptor;
use htap_common::encode_key;
use htap_common::error::Result;

use crate::ast::BoundStatement;

/// Execution route determined for a bound statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Catalog DDL operation (e.g., table creation).
    CatalogDdl,
    /// Transactional rowstore mutation (insert or delete).
    RowstoreWrite,
    /// Transactional rowstore point lookup by encoded primary key bytes.
    RowstorePointRead {
        /// Encoded primary key bytes.
        key: Vec<u8>,
    },
}

/// Classifies a catalog-bound statement into an execution route given the target storage descriptor.
///
/// # Semantics
/// - [`BoundStatement::CreateTable`] always routes to [`Route::CatalogDdl`], regardless of storage descriptor.
/// - [`BoundStatement::Insert`] and [`BoundStatement::Delete`] route to [`Route::RowstoreWrite`] for
///   [`StorageDescriptor::Row`], [`StorageDescriptor::Column`], and [`StorageDescriptor::Converting`],
///   as the rowstore remains authoritative for mutations during and after conversion.
/// - [`BoundStatement::Select`] routes to [`Route::RowstorePointRead`] with primary key bytes encoded via
///   [`htap_common::encode_key`] for [`StorageDescriptor::Row`], [`StorageDescriptor::Column`], and
///   [`StorageDescriptor::Converting`], as the rowstore remains authoritative for point reads during and
///   after conversion.
///
/// # Errors
/// Returns [`HtapError`] if primary key encoding fails.
pub fn classify_route(statement: &BoundStatement, storage: &StorageDescriptor) -> Result<Route> {
    match statement {
        BoundStatement::CreateTable(_) => Ok(Route::CatalogDdl),
        BoundStatement::Insert(_) | BoundStatement::Delete(_) => match storage {
            StorageDescriptor::Row
            | StorageDescriptor::Column
            | StorageDescriptor::Converting { .. } => Ok(Route::RowstoreWrite),
        },
        BoundStatement::Select(select) => match storage {
            StorageDescriptor::Row
            | StorageDescriptor::Column
            | StorageDescriptor::Converting { .. } => {
                let key = encode_key(&select.key)?;
                Ok(Route::RowstorePointRead { key })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{CreateTable, DeleteByPrimaryKey, Insert, PointSelect};
    use htap_catalog::StorageFormat;
    use htap_common::types::{ColumnDef, DataType, Row, Schema, Value};

    fn make_test_schema() -> Schema {
        Schema::new(vec![ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        }])
        .unwrap()
    }

    #[test]
    fn test_create_table_routes_to_ddl_regardless_of_storage() {
        let schema = make_test_schema();
        let stmt = BoundStatement::CreateTable(CreateTable::new("t", schema, vec![0]));

        let row = StorageDescriptor::Row;
        let col = StorageDescriptor::Column;
        let conv = StorageDescriptor::Converting {
            from: StorageFormat::Row,
            to: StorageFormat::Column,
            generation: 1,
        };

        assert_eq!(classify_route(&stmt, &row).unwrap(), Route::CatalogDdl);
        assert_eq!(classify_route(&stmt, &col).unwrap(), Route::CatalogDdl);
        assert_eq!(classify_route(&stmt, &conv).unwrap(), Route::CatalogDdl);
    }

    #[test]
    fn test_rowstore_write_routes() {
        let insert_stmt =
            BoundStatement::Insert(Insert::new("t", vec![Row::new(vec![Value::Int64(1)])]));
        let delete_stmt =
            BoundStatement::Delete(DeleteByPrimaryKey::new("t", vec![Value::Int64(1)]));

        let row = StorageDescriptor::Row;
        let col = StorageDescriptor::Column;
        let conv = StorageDescriptor::Converting {
            from: StorageFormat::Row,
            to: StorageFormat::Column,
            generation: 1,
        };

        // All storage descriptors route DML to RowstoreWrite (rowstore authoritative)
        assert_eq!(
            classify_route(&insert_stmt, &row).unwrap(),
            Route::RowstoreWrite
        );
        assert_eq!(
            classify_route(&insert_stmt, &col).unwrap(),
            Route::RowstoreWrite
        );
        assert_eq!(
            classify_route(&insert_stmt, &conv).unwrap(),
            Route::RowstoreWrite
        );

        assert_eq!(
            classify_route(&delete_stmt, &row).unwrap(),
            Route::RowstoreWrite
        );
        assert_eq!(
            classify_route(&delete_stmt, &col).unwrap(),
            Route::RowstoreWrite
        );
        assert_eq!(
            classify_route(&delete_stmt, &conv).unwrap(),
            Route::RowstoreWrite
        );
    }

    #[test]
    fn test_rowstore_point_read_route() {
        let key = vec![Value::Int64(42)];
        let select_stmt = BoundStatement::Select(PointSelect::new("t", vec![0], key.clone()));

        let row = StorageDescriptor::Row;
        let col = StorageDescriptor::Column;
        let conv = StorageDescriptor::Converting {
            from: StorageFormat::Row,
            to: StorageFormat::Column,
            generation: 1,
        };

        let expected_bytes = encode_key(&key).unwrap();

        // All storage descriptors route point reads to RowstorePointRead (rowstore authoritative)
        assert_eq!(
            classify_route(&select_stmt, &row).unwrap(),
            Route::RowstorePointRead {
                key: expected_bytes.clone()
            }
        );
        assert_eq!(
            classify_route(&select_stmt, &col).unwrap(),
            Route::RowstorePointRead {
                key: expected_bytes.clone()
            }
        );
        assert_eq!(
            classify_route(&select_stmt, &conv).unwrap(),
            Route::RowstorePointRead {
                key: expected_bytes
            }
        );
    }
}
