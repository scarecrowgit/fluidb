use std::collections::BTreeSet;
use std::sync::Arc;

use htap_catalog::model::{ColumnStats, TableStats};
use htap_common::error::{HtapError, Result};
use htap_common::types::{Row, Value};
use htap_common::Version;

use crate::memory_budget::{MemoryBudget, MemoryReservation};

/// Accumulates exact table and column statistics for one MVCC snapshot.
pub(crate) struct AnalyzeAccumulator {
    row_count: u64,
    columns: Vec<ColumnAccumulator>,
}

impl AnalyzeAccumulator {
    /// Creates an accumulator for a table with `column_count` columns.
    pub(crate) fn new(
        column_count: usize,
        distinct_limit: usize,
        memory_budget: Arc<MemoryBudget>,
    ) -> Self {
        let distinct_byte_limit = memory_budget.limit();
        Self {
            row_count: 0,
            columns: (0..column_count)
                .map(|_| {
                    ColumnAccumulator::new(
                        distinct_limit,
                        distinct_byte_limit,
                        Arc::clone(&memory_budget),
                    )
                })
                .collect(),
        }
    }

    /// Adds one logical table row to the statistics.
    pub(crate) fn observe_row(&mut self, row: &Row) -> Result<()> {
        if row.values().len() != self.columns.len() {
            return Err(HtapError::Internal(format!(
                "ANALYZE row has {} columns, expected {}",
                row.values().len(),
                self.columns.len()
            )));
        }
        self.row_count = self
            .row_count
            .checked_add(1)
            .ok_or(HtapError::CounterOverflow {
                counter: "analyze_row_count",
            })?;
        for (column, value) in self.columns.iter_mut().zip(row.values()) {
            column.observe(value)?;
        }
        Ok(())
    }

    /// Converts the accumulated values into catalog statistics.
    pub(crate) fn finish(self, analyzed_at_version: Version) -> TableStats {
        TableStats {
            analyzed_at_version: analyzed_at_version.get(),
            row_count: self.row_count,
            columns: self
                .columns
                .into_iter()
                .map(ColumnAccumulator::finish)
                .collect(),
        }
    }
}

/// Accumulates statistics for one column.
struct ColumnAccumulator {
    null_count: u64,
    min: Option<Value>,
    max: Option<Value>,
    min_reservation: Option<MemoryReservation>,
    max_reservation: Option<MemoryReservation>,
    distinct: Option<BTreeSet<Value>>,
    distinct_reservations: Vec<MemoryReservation>,
    distinct_limit: usize,
    distinct_byte_limit: usize,
    distinct_bytes: usize,
    memory_budget: Arc<MemoryBudget>,
}

impl ColumnAccumulator {
    fn new(
        distinct_limit: usize,
        distinct_byte_limit: usize,
        memory_budget: Arc<MemoryBudget>,
    ) -> Self {
        Self {
            null_count: 0,
            min: None,
            max: None,
            min_reservation: None,
            max_reservation: None,
            distinct: Some(BTreeSet::new()),
            distinct_reservations: Vec::new(),
            distinct_limit,
            distinct_byte_limit,
            distinct_bytes: 0,
            memory_budget,
        }
    }

    fn replace_bound(
        value: &Value,
        bound: &mut Option<Value>,
        reservation: &mut Option<MemoryReservation>,
        memory_budget: &Arc<MemoryBudget>,
    ) -> Result<()> {
        let replacement = memory_budget.try_reserve(MemoryBudget::estimate_value_bytes(value))?;
        // Retain the old reservation until the replacement allocation succeeds.
        *reservation = None;
        *bound = Some(value.clone());
        *reservation = Some(replacement);
        Ok(())
    }

    fn abandon_distinct(&mut self) {
        self.distinct = None;
        self.distinct_reservations.clear();
        self.distinct_bytes = 0;
    }

    fn observe(&mut self, value: &Value) -> Result<()> {
        if value.is_null() {
            self.null_count = self
                .null_count
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "analyze_null_count",
                })?;
            return Ok(());
        }

        let finite_bound = !matches!(value, Value::Float64(value) if !value.is_finite());
        if finite_bound && self.min.as_ref().is_none_or(|current| value < current) {
            Self::replace_bound(
                value,
                &mut self.min,
                &mut self.min_reservation,
                &self.memory_budget,
            )?;
        }
        if finite_bound && self.max.as_ref().is_none_or(|current| value > current) {
            Self::replace_bound(
                value,
                &mut self.max,
                &mut self.max_reservation,
                &self.memory_budget,
            )?;
        }

        if let Some(distinct) = &mut self.distinct {
            if !distinct.contains(value) {
                let value_bytes = MemoryBudget::estimate_value_bytes(value);
                if distinct.len() >= self.distinct_limit
                    || self.distinct_bytes.saturating_add(value_bytes) > self.distinct_byte_limit
                {
                    self.abandon_distinct();
                } else {
                    match self.memory_budget.try_reserve(value_bytes) {
                        Ok(reservation) => {
                            distinct.insert(value.clone());
                            self.distinct_bytes = self.distinct_bytes.saturating_add(value_bytes);
                            self.distinct_reservations.push(reservation);
                        }
                        Err(HtapError::InvalidArgument(_)) => self.abandon_distinct(),
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> ColumnStats {
        ColumnStats {
            null_count: self.null_count,
            distinct_count: self.distinct.map(|values| values.len() as u64),
            min: self.min,
            max: self.max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_collects_column_statistics() {
        let mut accumulator = AnalyzeAccumulator::new(2, 2, Arc::new(MemoryBudget::new(10_000)));
        accumulator
            .observe_row(&Row::new(vec![Value::Int64(2), Value::Null]))
            .unwrap();
        accumulator
            .observe_row(&Row::new(vec![Value::Int64(1), Value::String("a".into())]))
            .unwrap();
        accumulator
            .observe_row(&Row::new(vec![Value::Int64(2), Value::String("b".into())]))
            .unwrap();

        let stats = accumulator.finish(Version::new(7));
        assert_eq!(stats.analyzed_at_version, 7);
        assert_eq!(stats.row_count, 3);
        assert_eq!(
            stats.columns[0],
            ColumnStats {
                null_count: 0,
                distinct_count: Some(2),
                min: Some(Value::Int64(1)),
                max: Some(Value::Int64(2)),
            }
        );
        assert_eq!(
            stats.columns[1],
            ColumnStats {
                null_count: 1,
                distinct_count: Some(2),
                min: Some(Value::String("a".into())),
                max: Some(Value::String("b".into())),
            }
        );
    }

    #[test]
    fn accumulator_drops_distinct_values_after_byte_limit() {
        let first = Value::String("a".repeat(64));
        let second = Value::String("b".repeat(128));
        let first_bytes = MemoryBudget::estimate_value_bytes(&first);
        let second_bytes = MemoryBudget::estimate_value_bytes(&second);

        // Accommodate the initial min, max, and distinct copies of `first`,
        // plus the temporary replacement max allocation for `second`.
        // Once max is replaced, only `first_bytes` remains available, which
        // is insufficient to retain `second` as another distinct value.
        let budget = first_bytes
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add(second_bytes))
            .unwrap();
        let mut accumulator = AnalyzeAccumulator::new(1, 10, Arc::new(MemoryBudget::new(budget)));

        accumulator.observe_row(&Row::new(vec![first])).unwrap();
        accumulator.observe_row(&Row::new(vec![second])).unwrap();

        let stats = accumulator.finish(Version::new(1));
        assert_eq!(stats.columns[0].distinct_count, None);
    }

    #[test]
    fn accumulator_drops_distinct_values_after_limit() {
        let mut accumulator = AnalyzeAccumulator::new(1, 2, Arc::new(MemoryBudget::new(10_000)));
        for value in [1, 2, 3] {
            accumulator
                .observe_row(&Row::new(vec![Value::Int64(value)]))
                .unwrap();
        }

        let stats = accumulator.finish(Version::new(1));
        assert_eq!(stats.columns[0].distinct_count, None);
        assert_eq!(stats.columns[0].min, Some(Value::Int64(1)));
        assert_eq!(stats.columns[0].max, Some(Value::Int64(3)));
    }

    #[test]
    fn replace_bound_preserves_existing_reservation_when_budget_is_exhausted() {
        let original = Value::String("a".repeat(64));
        let replacement = Value::String("b".repeat(64));
        let original_bytes = MemoryBudget::estimate_value_bytes(&original);
        let replacement_bytes = MemoryBudget::estimate_value_bytes(&replacement);
        assert_eq!(original_bytes, replacement_bytes);

        let budget = Arc::new(MemoryBudget::new(original_bytes));
        let mut bound = None;
        let mut reservation = None;

        ColumnAccumulator::replace_bound(&original, &mut bound, &mut reservation, &budget).unwrap();

        let error =
            ColumnAccumulator::replace_bound(&replacement, &mut bound, &mut reservation, &budget)
                .unwrap_err();

        assert!(matches!(error, HtapError::InvalidArgument(_)));
        assert_eq!(bound, Some(original));
        assert!(reservation.is_some());

        // Dropping the retained reservation releases enough memory for the replacement.
        drop(reservation);
        ColumnAccumulator::replace_bound(&replacement, &mut bound, &mut None, &budget).unwrap();
    }
}
