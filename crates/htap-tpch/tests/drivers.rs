use std::sync::Arc;

use htap_sql::result::StatementResult;
use htap_tpch::{power_test, query_order, throughput_test, DriverError};

fn order_count(server: &Arc<htap_server::LocalServer>) -> usize {
    let mut session = server.open_session().expect("open session");
    match session
        .execute("SELECT o_orderkey FROM orders")
        .expect("count orders")
    {
        StatementResult::Query(result) => result.rows.len(),
        _ => panic!("orders query did not return rows"),
    }
}

#[test]
#[ignore]
fn power_driver_runs_all_queries_and_refreshes() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let server = Arc::new(htap_server::LocalServer::open(directory.path()).expect("open server"));
    let dataset = htap_tpch::generate("0.01", 42).expect("generate dataset");
    htap_tpch::load_dataset(
        &server,
        directory.path(),
        &dataset,
        &htap_tpch::LoadOptions::default(),
    )
    .expect("load dataset");

    let report = power_test(&server, "0.01", 1, 42).expect("run power driver");

    eprintln!(
        "rounded_interval_geomean_seconds: {}",
        report.rounded_interval_geomean_seconds
    );

    assert_eq!(
        report
            .query_timings
            .iter()
            .map(|timing| timing.query_number)
            .collect::<Vec<_>>(),
        query_order(0).to_vec()
    );
    assert!(report.rounded_interval_geomean_seconds > 0.0);
    assert_eq!(report.refresh_timings.len(), 1);
    assert_eq!(report.refresh_timings[0].refresh_key_stream, 1);
}

#[test]
#[ignore]
fn power_driver_rejects_invalid_refresh_key_stream_without_changes() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let server = Arc::new(htap_server::LocalServer::open(directory.path()).expect("open server"));
    let dataset = htap_tpch::generate("0.01", 42).expect("generate dataset");
    htap_tpch::load_dataset(
        &server,
        directory.path(),
        &dataset,
        &htap_tpch::LoadOptions::default(),
    )
    .expect("load dataset");

    let initial_orders = order_count(&server);

    for refresh_key_stream in [0, 1_001] {
        assert!(matches!(
            power_test(&server, "0.01", refresh_key_stream, 42),
            Err(DriverError::InvalidRefreshKeyRange)
        ));
        assert_eq!(order_count(&server), initial_orders);
    }
}

#[test]
#[ignore]
fn throughput_driver_runs_query_and_refresh_streams() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let server = Arc::new(htap_server::LocalServer::open(directory.path()).expect("open server"));
    let dataset = htap_tpch::generate("0.01", 42).expect("generate dataset");
    htap_tpch::load_dataset(
        &server,
        directory.path(),
        &dataset,
        &htap_tpch::LoadOptions::default(),
    )
    .expect("load dataset");

    let report = throughput_test(&server, "0.01", 2, 2, 42).expect("run throughput driver");

    eprintln!("measurement_interval: {}", report.measurement_interval);

    assert_eq!(report.streams.len(), 2);
    for stream in &report.streams {
        assert_eq!(stream.query_timings.len(), 22);
        assert_eq!(
            stream
                .query_timings
                .iter()
                .map(|timing| timing.query_number)
                .collect::<Vec<_>>(),
            query_order(stream.stream_id).to_vec()
        );
        assert_eq!(stream.refresh_timings.len(), 1);
        assert_eq!(
            stream.refresh_timings[0].refresh_key_stream,
            2 + stream.stream_id
        );
    }
    assert!(report.measurement_interval > 0.0);
}
