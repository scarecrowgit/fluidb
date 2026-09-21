use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use htap_common::error::{HtapError, Result};
use htap_common::types::{Row, Value};

/// Thread-safe memory budget shared by query execution tasks.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: usize,
    used: AtomicUsize,
}

impl MemoryBudget {
    /// Creates a memory budget with the specified byte limit.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// Reserves `bytes` from this budget, releasing them when the returned guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::InvalidArgument`] when the reservation would exceed the limit.
    pub fn try_reserve(self: &Arc<Self>, bytes: usize) -> Result<MemoryReservation> {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = used.checked_add(bytes) else {
                return Err(HtapError::InvalidArgument(format!(
                    "query memory budget exceeded: requested {bytes} bytes with {used} bytes used and {} bytes available",
                    self.limit.saturating_sub(used)
                )));
            };
            if next > self.limit {
                return Err(HtapError::InvalidArgument(format!(
                    "query memory budget exceeded: requested {bytes} bytes with {used} bytes used and {} bytes available",
                    self.limit.saturating_sub(used)
                )));
            }

            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    return Ok(MemoryReservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }

    /// Returns the total number of bytes available to this budget.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Returns the number of bytes remaining in this budget.
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used.load(Ordering::Acquire))
    }

    /// Estimates the memory occupied by a value, including owned string contents.
    pub fn estimate_value_bytes(value: &Value) -> usize {
        std::mem::size_of::<Value>()
            + match value {
                Value::String(value) => value.len(),
                Value::Bytes(value) => value.len(),
                _ => 0,
            }
    }

    /// Estimates the memory occupied by a row and its values.
    pub fn estimate_row_bytes(row: &Row) -> usize {
        std::mem::size_of::<Row>()
            + row
                .values()
                .iter()
                .map(Self::estimate_value_bytes)
                .sum::<usize>()
    }
}

/// RAII guard for bytes reserved from a [`MemoryBudget`].
#[derive(Debug)]
pub struct MemoryReservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reserve_within_limit() {
        let budget = Arc::new(MemoryBudget::new(100));
        let reservation = budget.try_reserve(60);
        assert!(reservation.is_ok());
        assert_eq!(budget.used.load(Ordering::Acquire), 60);
    }

    #[test]
    fn test_reserve_past_limit_fails() {
        let budget = Arc::new(MemoryBudget::new(100));
        let _reservation = budget.try_reserve(60).unwrap();
        let error = budget.try_reserve(41).unwrap_err();

        assert!(matches!(error, HtapError::InvalidArgument(_)));
        assert_eq!(budget.used.load(Ordering::Acquire), 60);
    }

    #[test]
    fn test_dropping_reservation_releases_bytes() {
        let budget = Arc::new(MemoryBudget::new(100));
        {
            let _reservation = budget.try_reserve(100).unwrap();
            assert_eq!(budget.used.load(Ordering::Acquire), 100);
        }

        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert!(budget.try_reserve(100).is_ok());
    }

    #[test]
    fn test_row_estimator_counts_strings() {
        let short = Row::new(vec![Value::String("a".to_string())]);
        let long = Row::new(vec![Value::String("a much longer string".to_string())]);

        assert!(MemoryBudget::estimate_row_bytes(&long) > MemoryBudget::estimate_row_bytes(&short));
        assert_eq!(
            MemoryBudget::estimate_row_bytes(&long) - MemoryBudget::estimate_row_bytes(&short),
            "a much longer string".len() - "a".len()
        );
    }
}
