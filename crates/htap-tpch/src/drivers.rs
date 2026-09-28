#![doc = r#"
TPC-H driver support.

This module reproduces the query-order permutations in TPC-H Specification Appendix A,
adapted from Moses & Oakford, *Tables of Random Permutations*, 1963, pp. 52-53.

The underlying server holds a process-wide execution lock for complete execute and commit
calls. Consequently, driver-level concurrent submissions and sessions do not make statements
execute in parallel. All streams use the fixed validation parameters rather than per-stream
random substitution parameters. This module computes no official TPC metric.
"#]

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

use crate::queries::{query, QueryError};
use crate::refresh::{rf1_new_sales, rf2_old_sales, RefreshError};
use crate::scale_factor::scale_factor;

const CENTISECOND_NANOS: u64 = 10_000_000;
const ROUNDING_HALF_CENTISECOND_NANOS: u64 = CENTISECOND_NANOS / 2;

const QUERY_ORDER: [[u8; 22]; 41] = [
    [
        14, 2, 9, 20, 6, 17, 18, 8, 21, 13, 3, 22, 16, 4, 11, 15, 1, 10, 19, 5, 7, 12,
    ],
    [
        21, 3, 18, 5, 11, 7, 6, 20, 17, 12, 16, 15, 13, 10, 2, 8, 14, 19, 9, 22, 1, 4,
    ],
    [
        6, 17, 14, 16, 19, 10, 9, 2, 15, 8, 5, 22, 12, 7, 13, 18, 1, 4, 20, 3, 11, 21,
    ],
    [
        8, 5, 4, 6, 17, 7, 1, 18, 22, 14, 9, 10, 15, 11, 20, 2, 21, 19, 13, 16, 12, 3,
    ],
    [
        5, 21, 14, 19, 15, 17, 12, 6, 4, 9, 8, 16, 11, 2, 10, 18, 1, 13, 7, 22, 3, 20,
    ],
    [
        21, 15, 4, 6, 7, 16, 19, 18, 14, 22, 11, 13, 3, 1, 2, 5, 8, 20, 12, 17, 10, 9,
    ],
    [
        10, 3, 15, 13, 6, 8, 9, 7, 4, 11, 22, 18, 12, 1, 5, 16, 2, 14, 19, 20, 17, 21,
    ],
    [
        18, 8, 20, 21, 2, 4, 22, 17, 1, 11, 9, 19, 3, 13, 5, 7, 10, 16, 6, 14, 15, 12,
    ],
    [
        19, 1, 15, 17, 5, 8, 9, 12, 14, 7, 4, 3, 20, 16, 6, 22, 10, 13, 2, 21, 18, 11,
    ],
    [
        8, 13, 2, 20, 17, 3, 6, 21, 18, 11, 19, 10, 15, 4, 22, 1, 7, 12, 9, 14, 5, 16,
    ],
    [
        6, 15, 18, 17, 12, 1, 7, 2, 22, 13, 21, 10, 14, 9, 3, 16, 20, 19, 11, 4, 8, 5,
    ],
    [
        15, 14, 18, 17, 10, 20, 16, 11, 1, 8, 4, 22, 5, 12, 3, 9, 21, 2, 13, 6, 19, 7,
    ],
    [
        1, 7, 16, 17, 18, 22, 12, 6, 8, 9, 11, 4, 2, 5, 20, 21, 13, 10, 19, 3, 14, 15,
    ],
    [
        21, 17, 7, 3, 1, 10, 12, 22, 9, 16, 6, 11, 2, 4, 5, 14, 8, 20, 13, 18, 15, 19,
    ],
    [
        2, 9, 5, 4, 18, 1, 20, 15, 16, 17, 7, 21, 13, 14, 19, 8, 22, 11, 10, 3, 12, 6,
    ],
    [
        16, 9, 17, 8, 14, 11, 10, 12, 6, 21, 7, 3, 15, 5, 22, 20, 1, 13, 19, 2, 4, 18,
    ],
    [
        1, 3, 6, 5, 2, 16, 14, 22, 17, 20, 4, 9, 10, 11, 15, 8, 12, 19, 18, 13, 7, 21,
    ],
    [
        3, 16, 5, 11, 21, 9, 2, 15, 10, 18, 17, 7, 8, 19, 14, 13, 1, 4, 22, 20, 6, 12,
    ],
    [
        14, 4, 13, 5, 21, 11, 8, 6, 3, 17, 2, 20, 1, 19, 10, 9, 12, 18, 15, 7, 22, 16,
    ],
    [
        4, 12, 22, 14, 5, 15, 16, 2, 8, 10, 17, 9, 21, 7, 3, 6, 13, 18, 11, 20, 19, 1,
    ],
    [
        16, 15, 14, 13, 4, 22, 18, 19, 7, 1, 12, 17, 5, 10, 20, 3, 9, 21, 11, 2, 6, 8,
    ],
    [
        20, 14, 21, 12, 15, 17, 4, 19, 13, 10, 11, 1, 16, 5, 18, 7, 8, 22, 9, 6, 3, 2,
    ],
    [
        16, 14, 13, 2, 21, 10, 11, 4, 1, 22, 18, 12, 19, 5, 7, 8, 6, 3, 15, 20, 9, 17,
    ],
    [
        18, 15, 9, 14, 12, 2, 8, 11, 22, 21, 16, 1, 6, 17, 5, 10, 19, 4, 20, 13, 3, 7,
    ],
    [
        7, 3, 10, 14, 13, 21, 18, 6, 20, 4, 9, 8, 22, 15, 2, 1, 5, 12, 19, 17, 11, 16,
    ],
    [
        18, 1, 13, 7, 16, 10, 14, 2, 19, 5, 21, 11, 22, 15, 8, 17, 20, 3, 4, 12, 6, 9,
    ],
    [
        13, 2, 22, 5, 11, 21, 20, 14, 7, 10, 4, 9, 19, 18, 6, 3, 1, 8, 15, 12, 17, 16,
    ],
    [
        14, 17, 21, 8, 2, 9, 6, 4, 5, 13, 22, 7, 15, 3, 1, 18, 16, 11, 10, 12, 20, 19,
    ],
    [
        10, 22, 1, 12, 13, 18, 21, 20, 2, 14, 16, 7, 15, 3, 4, 17, 5, 19, 6, 8, 9, 11,
    ],
    [
        10, 8, 9, 18, 12, 6, 1, 5, 20, 11, 17, 22, 16, 3, 13, 2, 15, 21, 14, 19, 7, 4,
    ],
    [
        7, 17, 22, 5, 3, 10, 13, 18, 9, 1, 14, 15, 21, 19, 16, 12, 8, 6, 11, 20, 4, 2,
    ],
    [
        2, 9, 21, 3, 4, 7, 1, 11, 16, 5, 20, 19, 18, 8, 17, 13, 10, 12, 15, 6, 14, 22,
    ],
    [
        15, 12, 8, 4, 22, 13, 16, 17, 18, 3, 7, 5, 6, 1, 9, 11, 21, 10, 14, 20, 19, 2,
    ],
    [
        15, 16, 2, 11, 17, 7, 5, 14, 20, 4, 21, 3, 10, 9, 12, 8, 13, 6, 18, 19, 22, 1,
    ],
    [
        1, 13, 11, 3, 4, 21, 6, 14, 15, 22, 18, 9, 7, 5, 10, 20, 12, 16, 17, 8, 19, 2,
    ],
    [
        14, 17, 22, 20, 8, 16, 5, 10, 1, 13, 2, 21, 12, 9, 4, 18, 3, 7, 6, 19, 15, 11,
    ],
    [
        9, 17, 7, 4, 5, 13, 21, 18, 11, 3, 22, 1, 6, 16, 20, 14, 15, 10, 8, 2, 12, 19,
    ],
    [
        13, 14, 5, 22, 19, 11, 9, 6, 18, 15, 8, 10, 7, 4, 17, 16, 3, 1, 12, 2, 21, 20,
    ],
    [
        20, 5, 4, 14, 11, 1, 6, 16, 8, 22, 7, 3, 2, 12, 21, 19, 17, 13, 10, 15, 18, 9,
    ],
    [
        3, 7, 14, 15, 6, 5, 21, 20, 18, 10, 4, 16, 19, 1, 13, 9, 8, 17, 11, 12, 22, 2,
    ],
    [
        13, 15, 17, 1, 22, 11, 3, 4, 7, 20, 14, 21, 9, 8, 2, 18, 16, 6, 10, 12, 5, 19,
    ],
];

pub const POWER_METRIC_LABEL: &str = "power-test rounded-interval geometric mean (seconds)";
pub const THROUGHPUT_METRIC_LABEL: &str = "throughput-test measurement interval (seconds)";

#[derive(Debug)]
pub enum DriverError {
    QueryError(QueryError),
    RefreshError(RefreshError),
    HtapError(HtapError),
    InvalidStreamCount,
    InvalidRefreshKeyRange,
    WorkerPanicked,
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueryError(error) => write!(f, "query driver error: {error}"),
            Self::RefreshError(error) => write!(f, "refresh driver error: {error}"),
            Self::HtapError(error) => write!(f, "HTAP driver error: {error}"),
            Self::InvalidStreamCount => f.write_str("invalid stream count"),
            Self::InvalidRefreshKeyRange => f.write_str("invalid refresh key stream range"),
            Self::WorkerPanicked => f.write_str("driver worker panicked"),
        }
    }
}

impl Error for DriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::QueryError(error) => Some(error),
            Self::RefreshError(error) => Some(error),
            Self::HtapError(error) => Some(error),
            Self::InvalidStreamCount | Self::InvalidRefreshKeyRange | Self::WorkerPanicked => None,
        }
    }
}

impl From<QueryError> for DriverError {
    fn from(error: QueryError) -> Self {
        Self::QueryError(error)
    }
}

impl From<RefreshError> for DriverError {
    fn from(error: RefreshError) -> Self {
        Self::RefreshError(error)
    }
}

impl From<HtapError> for DriverError {
    fn from(error: HtapError) -> Self {
        Self::HtapError(error)
    }
}

pub fn query_order(id: u32) -> &'static [u8; 22] {
    &QUERY_ORDER[(id % 41) as usize]
}

pub fn table_11_stream_count(sf: u32) -> Option<u32> {
    match sf {
        1 => Some(2),
        10 => Some(3),
        30 => Some(4),
        100 => Some(5),
        300 => Some(6),
        1_000 => Some(7),
        3_000 => Some(8),
        10_000 => Some(9),
        30_000 => Some(10),
        100_000 => Some(11),
        _ => None,
    }
}

pub(crate) fn round_to_nearest_centisecond(nanos: u64) -> u64 {
    let rounded = ((nanos as i128 + ROUNDING_HALF_CENTISECOND_NANOS as i128)
        / CENTISECOND_NANOS as i128)
        * CENTISECOND_NANOS as i128;
    u64::try_from(rounded)
        .unwrap_or(u64::MAX)
        .max(CENTISECOND_NANOS)
}

pub(crate) fn round_up_centisecond(nanos: u64) -> u64 {
    let rounded = ((nanos as i128 + CENTISECOND_NANOS as i128 - 1) / CENTISECOND_NANOS as i128)
        * CENTISECOND_NANOS as i128;
    u64::try_from(rounded)
        .unwrap_or(u64::MAX)
        .max(CENTISECOND_NANOS)
}

/// Timing data for one submitted TPC-H query.
#[derive(Debug, Clone)]
pub struct QueryTiming {
    pub query_number: u8,
    pub execute_duration: Duration,
    pub submission_interval: Duration,
    pub num_rows: u64,
}

/// Timing data for the RF1/RF2 pair associated with one refresh key stream.
#[derive(Debug, Clone)]
pub struct RefreshTiming {
    pub refresh_key_stream: u32,
    pub rf1_duration: Duration,
    pub rf2_duration: Duration,
}

/// Diagnostic result of a power-style driver run.
///
/// The caller must ensure that no other writer uses `server` while this run is
/// active. Conflicts are returned directly and are never retried.
#[derive(Debug, Clone)]
pub struct PowerTestReport {
    pub query_timings: Vec<QueryTiming>,
    pub refresh_timings: Vec<RefreshTiming>,
    pub rounded_interval_geomean_seconds: f64,
}

/// Diagnostic result for one submitted query stream.
#[derive(Debug, Clone)]
pub struct StreamReport {
    pub stream_id: u32,
    pub query_timings: Vec<QueryTiming>,
    pub refresh_timings: Vec<RefreshTiming>,
}

/// Diagnostic result of a throughput-style driver run.
#[derive(Debug, Clone)]
pub struct ThroughputTestReport {
    pub streams: Vec<StreamReport>,
    pub measurement_interval: f64,
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn rounded_seconds(duration: Duration) -> f64 {
    round_to_nearest_centisecond(duration_nanos(duration)) as f64 / 1_000_000_000.0
}

fn rounded_interval_geomean(intervals: impl IntoIterator<Item = Duration>) -> f64 {
    let intervals: Vec<_> = intervals.into_iter().collect();
    let mean_log = intervals
        .iter()
        .map(|interval| rounded_seconds(*interval).ln())
        .sum::<f64>()
        / intervals.len() as f64;
    mean_log.exp()
}

fn execute_queries(
    session: &mut htap_server::Session,
    sf: &str,
    order: &[u8; 22],
) -> Result<(Vec<QueryTiming>, Instant, Instant), DriverError> {
    let mut submitted = Vec::with_capacity(order.len());
    let mut completed = Vec::with_capacity(order.len());
    let mut timings = Vec::with_capacity(order.len());

    for &query_number in order {
        let sql = query(query_number, sf)?;
        let submission = Instant::now();
        let result = session.execute(&sql)?;
        let completion = Instant::now();
        let num_rows = match result {
            StatementResult::Query(result) => result.rows.len() as u64,
            _ => 0,
        };

        submitted.push(submission);
        completed.push(completion);
        timings.push(QueryTiming {
            query_number,
            execute_duration: completion.duration_since(submission),
            submission_interval: Duration::ZERO,
            num_rows,
        });
    }

    for index in 0..timings.len() {
        timings[index].submission_interval = if index + 1 == timings.len() {
            completed[index].duration_since(submitted[index])
        } else {
            submitted[index + 1].duration_since(submitted[index])
        };
    }

    Ok((
        timings,
        *submitted.first().expect("TPC-H query order is nonempty"),
        *completed.last().expect("TPC-H query order is nonempty"),
    ))
}

fn validate_scale_factor(sf: &str) -> Result<(), DriverError> {
    scale_factor(sf, 1)
        .map(|_| ())
        .map_err(|error| DriverError::HtapError(HtapError::InvalidArgument(error.to_string())))
}

/// Runs one power-style diagnostic sequence using fixed validation parameters.
pub fn power_test(
    server: &Arc<LocalServer>,
    sf: &str,
    refresh_key_stream: u32,
    seed: u64,
) -> Result<PowerTestReport, DriverError> {
    validate_scale_factor(sf)?;

    if !(1..=1_000).contains(&refresh_key_stream) {
        return Err(DriverError::InvalidRefreshKeyRange);
    }

    let mut refresh_session = server.open_session()?;
    let mut query_session = server.open_session()?;

    let rf1_start = Instant::now();
    rf1_new_sales(&mut refresh_session, sf, refresh_key_stream as u64, seed)?;
    let rf1_duration = rf1_start.elapsed();

    let (query_timings, _, _) = execute_queries(&mut query_session, sf, query_order(0))?;

    let rf2_start = Instant::now();
    rf2_old_sales(&mut refresh_session, sf, refresh_key_stream as u64)?;
    let rf2_duration = rf2_start.elapsed();

    let rounded_interval_geomean_seconds = rounded_interval_geomean(
        std::iter::once(rf1_duration)
            .chain(
                query_timings
                    .iter()
                    .map(|timing| timing.submission_interval),
            )
            .chain(std::iter::once(rf2_duration)),
    );

    Ok(PowerTestReport {
        query_timings,
        refresh_timings: vec![RefreshTiming {
            refresh_key_stream,
            rf1_duration,
            rf2_duration,
        }],
        rounded_interval_geomean_seconds,
    })
}

/// Runs concurrent submission streams using fixed validation parameters.
pub fn throughput_test(
    server: &Arc<LocalServer>,
    sf: &str,
    stream_count: u32,
    first_refresh_key_stream: u32,
    seed: u64,
) -> Result<ThroughputTestReport, DriverError> {
    validate_scale_factor(sf)?;

    if stream_count == 0 {
        return Err(DriverError::InvalidStreamCount);
    }
    if first_refresh_key_stream == 0 {
        return Err(DriverError::InvalidRefreshKeyRange);
    }
    let last_refresh_key_stream = first_refresh_key_stream
        .checked_add(stream_count - 1)
        .ok_or(DriverError::InvalidRefreshKeyRange)?;
    if last_refresh_key_stream > 1_000 {
        return Err(DriverError::InvalidRefreshKeyRange);
    }

    let barrier = Arc::new(Barrier::new(stream_count as usize + 1));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut query_handles = Vec::with_capacity(stream_count as usize);

    for stream_id in 0..stream_count {
        let server = Arc::clone(server);
        let barrier = Arc::clone(&barrier);
        let cancel = Arc::clone(&cancel);
        let sf = sf.to_owned();

        query_handles.push((
            stream_id,
            thread::spawn(move || {
                // Every spawned thread must reach the barrier exactly once to prevent deadlock.
                let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || -> Result<(htap_server::Session, [u8; 22]), DriverError> {
                        Ok((server.open_session()?, *query_order(stream_id)))
                    },
                ))
                .unwrap_or(Err(DriverError::WorkerPanicked));

                barrier.wait();

                let (mut session, order) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        cancel.store(true, Ordering::Release);
                        return Err(error);
                    }
                };

                let result = execute_queries(&mut session, &sf, &order).map(
                    |(query_timings, first_submission, last_completion)| {
                        (
                            StreamReport {
                                stream_id,
                                query_timings,
                                refresh_timings: Vec::new(),
                            },
                            first_submission,
                            last_completion,
                        )
                    },
                );
                if result.is_err() {
                    cancel.store(true, Ordering::Release);
                }
                result
            }),
        ));
    }

    let refresh_server = Arc::clone(server);
    let refresh_barrier = Arc::clone(&barrier);
    let refresh_cancel = Arc::clone(&cancel);
    let refresh_sf = sf.to_owned();
    let refresh_handle = thread::spawn(move || {
        // Every spawned thread must reach the barrier exactly once to prevent deadlock.
        let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || -> Result<htap_server::Session, DriverError> { Ok(refresh_server.open_session()?) },
        ))
        .unwrap_or(Err(DriverError::WorkerPanicked));

        refresh_barrier.wait();

        let mut session = match prepared {
            Ok(session) => session,
            Err(error) => {
                refresh_cancel.store(true, Ordering::Release);
                return Err(error);
            }
        };

        let result = (|| -> Result<_, DriverError> {
            let mut timings = Vec::with_capacity(stream_count as usize);
            let mut first_submission = None;
            let mut last_completion = None;

            for stream_offset in 0..stream_count {
                if refresh_cancel.load(Ordering::Acquire) {
                    break;
                }

                let refresh_key_stream = first_refresh_key_stream + stream_offset;
                let rf1_submission = Instant::now();
                first_submission.get_or_insert(rf1_submission);
                rf1_new_sales(&mut session, &refresh_sf, refresh_key_stream as u64, seed)?;
                let rf1_duration = rf1_submission.elapsed();

                let rf2_submission = Instant::now();
                rf2_old_sales(&mut session, &refresh_sf, refresh_key_stream as u64)?;
                let rf2_duration = rf2_submission.elapsed();
                last_completion = Some(Instant::now());

                timings.push(RefreshTiming {
                    refresh_key_stream,
                    rf1_duration,
                    rf2_duration,
                });
            }

            Ok((timings, first_submission, last_completion))
        })();

        if result.is_err() {
            refresh_cancel.store(true, Ordering::Release);
        }
        result
    });

    let refresh_result = match refresh_handle.join() {
        Ok(result) => result,
        Err(_) => Err(DriverError::WorkerPanicked),
    };

    let mut query_results = Vec::with_capacity(stream_count as usize);
    for (stream_id, handle) in query_handles {
        let result = match handle.join() {
            Ok(result) => result,
            Err(_) => Err(DriverError::WorkerPanicked),
        };
        query_results.push((stream_id, result));
    }

    let (refresh_timings, refresh_first_submission, refresh_last_completion) = refresh_result?;
    let mut streams = Vec::with_capacity(stream_count as usize);
    let mut earliest_submission = refresh_first_submission;
    let mut latest_completion = refresh_last_completion;

    query_results.sort_by_key(|(stream_id, _)| *stream_id);
    for (_, result) in query_results {
        let (mut stream, first_submission, last_completion) = result?;
        stream.refresh_timings = refresh_timings
            .iter()
            .filter(|timing| {
                timing.refresh_key_stream == first_refresh_key_stream + stream.stream_id
            })
            .cloned()
            .collect();

        earliest_submission = Some(match earliest_submission {
            Some(current) => current.min(first_submission),
            None => first_submission,
        });
        latest_completion = Some(match latest_completion {
            Some(current) => current.max(last_completion),
            None => last_completion,
        });
        streams.push(stream);
    }

    let measurement = latest_completion
        .expect("at least one query stream completes")
        .duration_since(earliest_submission.expect("at least one submission occurs"));
    let rounded_nanos = round_up_centisecond(duration_nanos(measurement));

    Ok(ThroughputTestReport {
        streams,
        measurement_interval: rounded_nanos as f64 / 1_000_000_000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rounded_interval_geomean() {
        let actual = rounded_interval_geomean([
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
        ]);
        let expected = (0.01_f64 * 0.02 * 0.03).powf(1.0 / 3.0);
        assert!((actual - expected).abs() < 1e-12);
    }

    #[test]
    fn test_throughput_rejects_invalid_inputs_before_workers() {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let server = Arc::new(LocalServer::open(directory.path()).expect("open local server"));

        assert!(matches!(
            throughput_test(&server, "0.01", 0, 1, 42),
            Err(DriverError::InvalidStreamCount)
        ));
        assert!(matches!(
            throughput_test(&server, "0.01", 2, 0, 42),
            Err(DriverError::InvalidRefreshKeyRange)
        ));
        assert!(matches!(
            throughput_test(&server, "0.01", 2, 1_000, 42),
            Err(DriverError::InvalidRefreshKeyRange)
        ));
    }

    #[test]
    fn test_query_order_wraparound() {
        assert_eq!(query_order(41), query_order(0));
        assert_eq!(query_order(82), query_order(0));
        assert_eq!(query_order(42), query_order(1));
    }

    #[test]
    fn test_table_11_stream_count_exact_values() {
        for (sf, streams) in [
            (1, 2),
            (10, 3),
            (30, 4),
            (100, 5),
            (300, 6),
            (1_000, 7),
            (3_000, 8),
            (10_000, 9),
            (30_000, 10),
            (100_000, 11),
        ] {
            assert_eq!(table_11_stream_count(sf), Some(streams));
        }

        for sf in [0, 2, 500, 200_000] {
            assert_eq!(table_11_stream_count(sf), None);
        }
    }

    #[test]
    fn test_labels_do_not_contain_official_metrics() {
        for label in [POWER_METRIC_LABEL, THROUGHPUT_METRIC_LABEL] {
            let label = label.to_ascii_lowercase();
            for forbidden in [
                "qphh",
                "qpph",
                "qthh",
                "price",
                "performance",
                "composite",
                "query-per-hour",
            ] {
                assert!(!label.contains(forbidden), "{label} contains {forbidden}");
            }
        }
    }

    #[test]
    fn test_rounding_boundaries() {
        assert_eq!(round_to_nearest_centisecond(0), 10_000_000);
        assert_eq!(round_to_nearest_centisecond(1_000), 10_000_000);
        assert_eq!(round_to_nearest_centisecond(4_999_999), 10_000_000);
        assert_eq!(round_to_nearest_centisecond(5_000_000), 10_000_000);
        assert_eq!(round_to_nearest_centisecond(14_999_999), 10_000_000);
        assert_eq!(round_to_nearest_centisecond(15_000_000), 20_000_000);
    }

    #[test]
    fn test_round_up_centisecond_worked_example() {
        assert_eq!(round_up_centisecond(923_741_666_667), 923_750_000_000);
    }

    #[test]
    fn test_rounding_saturates_at_u64_max() {
        // Values above the largest representable centisecond round to u64::MAX.
        assert_eq!(round_to_nearest_centisecond(u64::MAX), u64::MAX);
        assert_eq!(round_up_centisecond(u64::MAX), u64::MAX);
    }

    #[test]
    fn test_permutation_row_coverage() {
        assert_eq!(
            QUERY_ORDER[0],
            [14, 2, 9, 20, 6, 17, 18, 8, 21, 13, 3, 22, 16, 4, 11, 15, 1, 10, 19, 5, 7, 12]
        );
        assert_eq!(
            QUERY_ORDER[1],
            [21, 3, 18, 5, 11, 7, 6, 20, 17, 12, 16, 15, 13, 10, 2, 8, 14, 19, 9, 22, 1, 4]
        );
        assert_eq!(
            QUERY_ORDER[40],
            [13, 15, 17, 1, 22, 11, 3, 4, 7, 20, 14, 21, 9, 8, 2, 18, 16, 6, 10, 12, 5, 19]
        );
    }

    #[test]
    fn test_every_row_is_permutation() {
        for row in QUERY_ORDER {
            let mut values = row;
            values.sort_unstable();
            assert_eq!(values, std::array::from_fn(|index| (index + 1) as u8));
        }
    }
}
