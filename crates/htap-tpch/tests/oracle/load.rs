use std::sync::Arc;

use htap_server::LocalServer;
use tempfile::TempDir;

pub fn load_at(scale_factor: &str, seed: u64) -> (TempDir, Arc<LocalServer>, htap_tpch::Dataset) {
    let dataset = htap_tpch::generate(scale_factor, seed).expect("generate TPC-H dataset");
    let directory = TempDir::new().expect("create temporary fixture directory");
    let server = Arc::new(LocalServer::open(directory.path()).expect("open fixture server"));

    htap_tpch::load_dataset(
        &server,
        directory.path(),
        &dataset,
        &htap_tpch::LoadOptions::default(),
    )
    .expect("load generated TPC-H dataset");

    (directory, server, dataset)
}
