//! Core relational data model: [`DataType`], [`Value`], [`ColumnDef`],
//! [`Schema`] and [`Row`].
//!
//! These types are the vocabulary shared by the row store, the column store
//! and the SQL layer, so their ordering and equality semantics are part of the
//! engine's on-disk contract (see [`crate::keycodec`]).

use crate::error::{HtapError, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Logical type of a column.
///
/// The declaration order of the variants is significant: it defines the
/// tie-breaking order used when [`Value`]s of different types are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataType {
    /// Boolean.
    Bool,
    /// Signed 32-bit integer.
    Int32,
    /// Signed 64-bit integer.
    Int64,
    /// IEEE-754 double precision float.
    Float64,
    /// UTF-8 string.
    String,
    /// Opaque byte string.
    Bytes,
    /// Microseconds since the Unix epoch, stored as an `i64`.
    Timestamp,
}

impl DataType {
    /// SQL-facing name of the type.
    pub fn name(&self) -> &'static str {
        match self {
            DataType::Bool => "bool",
            DataType::Int32 => "int",
            DataType::Int64 => "bigint",
            DataType::Float64 => "double",
            DataType::String => "varchar",
            DataType::Bytes => "varbinary",
            DataType::Timestamp => "timestamp",
        }
    }

    /// Discriminant rank, used as the deterministic tie-breaker when comparing
    /// two non-null values of different types.
    #[inline]
    fn rank(self) -> u8 {
        match self {
            DataType::Bool => 0,
            DataType::Int32 => 1,
            DataType::Int64 => 2,
            DataType::Float64 => 3,
            DataType::String => 4,
            DataType::Bytes => 5,
            DataType::Timestamp => 6,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A single dynamically typed value.
///
/// # Ordering and equality
///
/// `Value` is used as (part of) an index key, and an index key **must** have a
/// total order: every pair of values has to compare as exactly one of
/// `Less`/`Equal`/`Greater`, and `Eq`, `Hash` and `Ord` must all agree.
///
/// IEEE-754 float semantics are incompatible with that requirement, because
/// `NaN != NaN` and `NaN` is unordered with respect to everything. We therefore
/// **deliberately deviate from IEEE-754**: `Float64` is compared with
/// [`f64::total_cmp`], which yields the totally ordered
/// `-NaN < -inf < .. < -0.0 < 0.0 < .. < +inf < +NaN`. As a consequence
/// `Value::Float64(f64::NAN) == Value::Float64(f64::NAN)` holds, and `-0.0` and
/// `0.0` are *not* equal to each other. `Hash` matches this by hashing
/// `f64::to_bits()`.
///
/// `PartialEq` is hand-written for exactly this reason: the derived
/// implementation would delegate to `f64`'s IEEE comparison, making `NaN` not
/// equal to itself and thereby violating the `Eq`/`Hash`/`Ord` contracts.
///
/// `Null` sorts before every non-null value. Two non-null values of different
/// types are ordered by their [`DataType`] discriminant, which keeps the
/// ordering total and deterministic even for heterogeneous input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    /// SQL NULL. Sorts before every non-null value.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed 32-bit integer.
    Int32(i32),
    /// Signed 64-bit integer.
    Int64(i64),
    /// IEEE-754 double, totally ordered via [`f64::total_cmp`].
    Float64(f64),
    /// UTF-8 string.
    String(String),
    /// Opaque byte string.
    Bytes(Vec<u8>),
    /// Microseconds since the Unix epoch.
    Timestamp(i64),
}

impl Value {
    /// Logical type of this value, or `None` for [`Value::Null`] (a NULL
    /// carries no type of its own; the column definition supplies it).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(DataType::Bool),
            Value::Int32(_) => Some(DataType::Int32),
            Value::Int64(_) => Some(DataType::Int64),
            Value::Float64(_) => Some(DataType::Float64),
            Value::String(_) => Some(DataType::String),
            Value::Bytes(_) => Some(DataType::Bytes),
            Value::Timestamp(_) => Some(DataType::Timestamp),
        }
    }

    /// Whether this value is [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Sort rank: `0` for NULL, otherwise the [`DataType`] rank shifted by one
    /// so that NULL always sorts first.
    #[inline]
    fn sort_rank(&self) -> u8 {
        match self.data_type() {
            None => 0,
            Some(dt) => dt.rank() + 1,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int32(v) => write!(f, "{v}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::String(v) => write!(f, "{v}"),
            Value::Bytes(v) => {
                for b in v {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
            Value::Timestamp(v) => write!(f, "{v}"),
        }
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int32(a), Value::Int32(b)) => a.cmp(b),
            (Value::Int64(a), Value::Int64(b)) => a.cmp(b),
            // Total order over floats, NaN included. See the type docs.
            (Value::Float64(a), Value::Float64(b)) => a.total_cmp(b),
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            // Different variants: fall back to the discriminant order so the
            // relation stays total and deterministic.
            _ => self.sort_rank().cmp(&other.sort_rank()),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Hand-written so that `Float64(NaN) == Float64(NaN)`; deriving `PartialEq`
// would use IEEE-754 semantics and break the `Eq`/`Hash`/`Ord` contract.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.sort_rank().hash(state);
        match self {
            Value::Null => {}
            Value::Bool(v) => v.hash(state),
            Value::Int32(v) => v.hash(state),
            Value::Int64(v) => v.hash(state),
            // Hash the raw bits so equal-by-`total_cmp` floats hash equally.
            Value::Float64(v) => v.to_bits().hash(state),
            Value::String(v) => v.hash(state),
            Value::Bytes(v) => v.hash(state),
            Value::Timestamp(v) => v.hash(state),
        }
    }
}

/// Definition of a single column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnDef {
    /// Column name, unique within a [`Schema`].
    pub name: String,
    /// Logical type.
    pub data_type: DataType,
    /// Whether NULL is permitted.
    pub nullable: bool,
    /// Whether the column participates in the primary key.
    pub primary_key: bool,
}

/// An ordered list of [`ColumnDef`]s with unique names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    columns: Vec<ColumnDef>,
}

impl Schema {
    /// Build a schema, validating that it is non-empty and that all column
    /// names are unique.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::InvalidArgument`] if `columns` is empty or contains
    /// duplicate column names.
    pub fn new(columns: Vec<ColumnDef>) -> Result<Schema> {
        if columns.is_empty() {
            return Err(HtapError::InvalidArgument(
                "schema must have at least one column".into(),
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(columns.len());
        for col in &columns {
            if !seen.insert(col.name.as_str()) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate column name: {}",
                    col.name
                )));
            }
        }
        Ok(Schema { columns })
    }

    /// All columns, in declaration order.
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// Number of columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Always `false`: a valid schema has at least one column. Present to
    /// satisfy the usual `len`/`is_empty` pairing.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Index of the column with the given name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Column at `idx`.
    pub fn column(&self, idx: usize) -> Option<&ColumnDef> {
        self.columns.get(idx)
    }

    /// Indices of the primary key columns, in declaration order.
    pub fn primary_key_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect()
    }
}

/// A tuple of [`Value`]s, positionally matching a [`Schema`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    values: Vec<Value>,
}

impl Row {
    /// Create a row from its values.
    pub fn new(values: Vec<Value>) -> Row {
        Row { values }
    }

    /// The values, in column order.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Value at `idx`.
    pub fn get(&self, idx: usize) -> Option<&Value> {
        self.values.get(idx)
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the row has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Consume the row and return the owned values.
    pub fn into_values(self) -> Vec<Value> {
        self.values
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::collections::HashSet;

    fn hash_of(v: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    fn col(name: &str, data_type: DataType, primary_key: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            nullable: false,
            primary_key,
        }
    }

    #[test]
    fn test_data_type_names_and_display() {
        assert_eq!(DataType::Bool.name(), "bool");
        assert_eq!(DataType::Int32.name(), "int");
        assert_eq!(DataType::Int64.name(), "bigint");
        assert_eq!(DataType::Float64.name(), "double");
        assert_eq!(DataType::String.name(), "varchar");
        assert_eq!(DataType::Bytes.name(), "varbinary");
        assert_eq!(DataType::Timestamp.name(), "timestamp");
        assert_eq!(DataType::Timestamp.to_string(), "timestamp");
    }

    #[test]
    fn test_value_display_and_data_type() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Int32(-7).to_string(), "-7");
        assert_eq!(Value::String("hi".into()).to_string(), "hi");
        assert_eq!(Value::Bytes(vec![0x00, 0xff]).to_string(), "00ff");

        assert_eq!(Value::Null.data_type(), None);
        assert!(Value::Null.is_null());
        assert!(!Value::Int64(0).is_null());
        assert_eq!(Value::Int64(1).data_type(), Some(DataType::Int64));
        assert_eq!(Value::Timestamp(1).data_type(), Some(DataType::Timestamp));
    }

    #[test]
    fn test_null_sorts_before_every_non_null() {
        let non_null = [
            Value::Bool(false),
            Value::Int32(i32::MIN),
            Value::Int64(i64::MIN),
            Value::Float64(f64::NEG_INFINITY),
            Value::String(String::new()),
            Value::Bytes(Vec::new()),
            Value::Timestamp(i64::MIN),
        ];
        for v in &non_null {
            assert!(Value::Null < *v, "NULL should sort before {v:?}");
            assert!(*v > Value::Null);
        }
        assert_eq!(Value::Null, Value::Null);
    }

    #[test]
    fn test_value_ordering_same_type() {
        assert!(Value::Int64(-1) < Value::Int64(0));
        assert!(Value::Int32(i32::MIN) < Value::Int32(i32::MAX));
        assert!(Value::Bool(false) < Value::Bool(true));
        assert!(Value::String("a".into()) < Value::String("b".into()));
        assert!(Value::Bytes(vec![1]) < Value::Bytes(vec![1, 0]));
        assert!(Value::Timestamp(-5) < Value::Timestamp(5));
    }

    #[test]
    fn test_float_total_ordering_with_nan() {
        let mut floats = vec![
            Value::Float64(f64::NAN),
            Value::Float64(1.0),
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(0.0),
            Value::Float64(-0.0),
            Value::Float64(f64::INFINITY),
            Value::Float64(-1.0),
        ];
        floats.sort();
        assert_eq!(
            floats,
            vec![
                Value::Float64(f64::NEG_INFINITY),
                Value::Float64(-1.0),
                Value::Float64(-0.0),
                Value::Float64(0.0),
                Value::Float64(1.0),
                Value::Float64(f64::INFINITY),
                Value::Float64(f64::NAN),
            ]
        );
        // total_cmp distinguishes -0.0 from 0.0.
        assert!(Value::Float64(-0.0) < Value::Float64(0.0));
        assert_ne!(Value::Float64(-0.0), Value::Float64(0.0));
    }

    #[test]
    fn test_nan_equality_and_hash_consistency() {
        let a = Value::Float64(f64::NAN);
        let b = Value::Float64(f64::NAN);
        // Deliberate deviation from IEEE-754: NaN equals itself here.
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(hash_of(&a), hash_of(&b));

        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b);
        assert_eq!(set.len(), 1, "two NaNs must dedupe in a HashSet");

        set.insert(Value::Float64(0.0));
        set.insert(Value::Float64(0.0));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_cross_type_ordering_is_total_and_deterministic() {
        // Ordered by DataType discriminant: Bool < Int32 < Int64 < Float64 <
        // String < Bytes < Timestamp, with NULL first.
        let mut vals = vec![
            Value::Timestamp(0),
            Value::Bytes(vec![0]),
            Value::String("z".into()),
            Value::Float64(9.0),
            Value::Int64(9),
            Value::Int32(9),
            Value::Bool(true),
            Value::Null,
        ];
        vals.sort();
        assert_eq!(
            vals,
            vec![
                Value::Null,
                Value::Bool(true),
                Value::Int32(9),
                Value::Int64(9),
                Value::Float64(9.0),
                Value::String("z".into()),
                Value::Bytes(vec![0]),
                Value::Timestamp(0),
            ]
        );
        // Numeric equality across types does NOT hold; type rank decides.
        assert_ne!(Value::Int32(9), Value::Int64(9));
        assert!(Value::Int32(i32::MAX) < Value::Int64(i64::MIN));
    }

    #[test]
    fn test_schema_new_rejects_duplicates_and_empty() {
        let err = Schema::new(vec![]).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(err.to_string().contains("at least one column"));

        let dup = Schema::new(vec![
            col("id", DataType::Int64, true),
            col("id", DataType::String, false),
        ])
        .unwrap_err();
        assert!(matches!(dup, HtapError::InvalidArgument(_)));
        assert!(dup.to_string().contains("duplicate column name: id"));
    }

    #[test]
    fn test_schema_accessors_and_primary_key_indices() {
        let schema = Schema::new(vec![
            col("tenant", DataType::String, true),
            col("payload", DataType::Bytes, false),
            col("id", DataType::Int64, true),
        ])
        .unwrap();

        assert_eq!(schema.len(), 3);
        assert!(!schema.is_empty());
        assert_eq!(schema.columns().len(), 3);
        assert_eq!(schema.column_index("id"), Some(2));
        assert_eq!(schema.column_index("missing"), None);
        assert_eq!(schema.column(1).unwrap().name, "payload");
        assert_eq!(schema.column(3), None);
        // Declaration order, not sorted by name.
        assert_eq!(schema.primary_key_indices(), vec![0, 2]);

        let no_pk = Schema::new(vec![col("a", DataType::Bool, false)]).unwrap();
        assert!(no_pk.primary_key_indices().is_empty());
    }

    #[test]
    fn test_row_accessors() {
        let row = Row::new(vec![Value::Int64(1), Value::Null]);
        assert_eq!(row.len(), 2);
        assert!(!row.is_empty());
        assert_eq!(row.get(0), Some(&Value::Int64(1)));
        assert_eq!(row.get(1), Some(&Value::Null));
        assert_eq!(row.get(2), None);
        assert_eq!(row.values(), &[Value::Int64(1), Value::Null]);
        assert_eq!(
            row.clone().into_values(),
            vec![Value::Int64(1), Value::Null]
        );
        assert!(Row::new(vec![]).is_empty());
    }

    #[test]
    fn test_serde_roundtrip() {
        let schema = Schema::new(vec![col("id", DataType::Int64, true)]).unwrap();
        let json = serde_json::to_string(&schema).unwrap();
        assert_eq!(schema, serde_json::from_str::<Schema>(&json).unwrap());

        let row = Row::new(vec![
            Value::Null,
            Value::Float64(1.5),
            Value::String("x".into()),
        ]);
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(row, serde_json::from_str::<Row>(&json).unwrap());
    }
}
