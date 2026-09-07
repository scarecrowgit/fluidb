//! Crash-test victim for `tests/wal_crash.rs`.
//!
//! Usage: `wal_crash_child <dir> <count> <puts_per_txn>`
//!
//! Opens a WAL in `<dir>` and appends committed transactions in a tight loop
//! until it has written `<count>` of them, or forever if `<count>` is 0. Each
//! transaction writes `<puts_per_txn>` `Put` records and then a `Commit`,
//! fsynced.
//!
//! **The contract with the test:** a txn id is printed to stdout, one per line
//! and flushed, only *after* `append_commit` has returned, i.e. only after the
//! commit record is durable. So every id the parent manages to read must
//! survive a `SIGKILL`, no matter when the kill lands. The process is expected
//! to be killed mid-transaction; that is the point. A large `puts_per_txn`
//! widens the window in which the kill lands between a transaction's data
//! records and its commit marker, which is exactly the case recovery has to
//! get right.
#![forbid(unsafe_code)]

use std::io::Write;

use htap_common::{Row, Value, Version};
use htap_rowstore::{Wal, WalOptions, WalRecord};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const USAGE: &str = "usage: wal_crash_child <dir> <count> [puts_per_txn]  \
                         (count 0 = run forever)";
    let mut args = std::env::args().skip(1);
    let dir = args.next().ok_or(USAGE)?;
    let count: u64 = args.next().ok_or(USAGE)?.parse()?;
    let puts_per_txn: u64 = match args.next() {
        Some(s) => s.parse()?,
        None => 2,
    };

    let mut wal = Wal::open(WalOptions::new(&dir).with_sync_on_commit(true))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut txn_id: u64 = 1;
    while count == 0 || txn_id <= count {
        let version = Version::new(txn_id + 1);
        for col in 0..puts_per_txn {
            wal.append(&WalRecord::Put {
                txn_id,
                partition_id: col,
                key: format!("txn-{txn_id}-key-{col}").into_bytes(),
                row: Row::new(vec![
                    Value::Int64(txn_id as i64),
                    Value::String(format!("payload-{txn_id}-{col}")),
                ]),
                version,
            })?;
        }
        // Durable when this returns.
        wal.append_commit(&WalRecord::Commit { txn_id, version })?;

        // Only now may the parent be told the txn committed.
        writeln!(out, "{txn_id}")?;
        out.flush()?;

        txn_id += 1;
    }
    Ok(())
}
