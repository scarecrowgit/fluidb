//! Crash-test victim for `tests/engine_crash.rs`.
//!
//! Usage: `engine_crash_child <dir> <count> [flush_every]`
//!
//! Opens an `Engine` in `<dir>` and commits monotonically numbered rows in a tight loop
//! until it has committed `<count>` of them, or forever if `<count>` is 0.
//! Calls `flush()` every `flush_every` commits (default 5).
//!
//! **The contract with the test:** a txn id is printed to stdout, one per line
//! and flushed, only *after* `Engine::commit` has returned successfully.
//! Every id the parent reads must be recovered upon reopening the Engine.
#![forbid(unsafe_code)]

use std::io::Write;

use htap_common::{Row, Value};
use htap_rowstore::{Engine, EngineOptions, Mutation};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const USAGE: &str = "usage: engine_crash_child <dir> <count> [flush_every] \
                         (count 0 = run forever)";
    let mut args = std::env::args().skip(1);
    let dir = args.next().ok_or(USAGE)?;
    let count: u64 = args.next().ok_or(USAGE)?.parse()?;
    let flush_every: u64 = match args.next() {
        Some(s) => s.parse()?,
        None => 5,
    };

    let options = EngineOptions::new(&dir);
    let engine = Engine::open(options)?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut txn_id: u64 = 1;
    while count == 0 || txn_id <= count {
        let snapshot = engine.snapshot();
        let key = format!("txn-key-{txn_id}").into_bytes();
        let row = Row::new(vec![
            Value::Int64(txn_id as i64),
            Value::String(format!("engine-val-{txn_id}")),
        ]);

        engine.commit(
            txn_id,
            snapshot,
            vec![Mutation::Put {
                partition_id: 0,
                key,
                row,
            }],
        )?;

        if flush_every > 0 && txn_id.is_multiple_of(flush_every) {
            engine.flush()?;
        }

        // Only now may the parent be told the txn committed.
        writeln!(out, "{txn_id}")?;
        out.flush()?;

        txn_id += 1;
    }

    Ok(())
}
