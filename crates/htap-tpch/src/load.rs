use std::io::{self, Read};
use std::path::Path;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::{HtapError, Result};
use htap_movement::{CopyOptions, CopyReport, DataFormat};
use htap_server::LocalServer;
use htap_sql::result::{CommandResult, StatementResult};

use crate::generate::{
    Customer, Dataset, Lineitem, Nation, Orders, Part, Partsupp, Region, Supplier,
};
use crate::schema::{ddl_statements, TABLE_NAMES};

const CSV_REFILL_BYTES: usize = 64 * 1024;

const REGION_HEADERS: [&str; 3] = ["r_regionkey", "r_name", "r_comment"];
const NATION_HEADERS: [&str; 4] = ["n_nationkey", "n_name", "n_regionkey", "n_comment"];
const PART_HEADERS: [&str; 9] = [
    "p_partkey",
    "p_name",
    "p_mfgr",
    "p_brand",
    "p_type",
    "p_size",
    "p_container",
    "p_retailprice",
    "p_comment",
];
const SUPPLIER_HEADERS: [&str; 7] = [
    "s_suppkey",
    "s_name",
    "s_address",
    "s_nationkey",
    "s_phone",
    "s_acctbal",
    "s_comment",
];
const PARTSUPP_HEADERS: [&str; 5] = [
    "ps_partkey",
    "ps_suppkey",
    "ps_availqty",
    "ps_supplycost",
    "ps_comment",
];
const CUSTOMER_HEADERS: [&str; 8] = [
    "c_custkey",
    "c_name",
    "c_address",
    "c_nationkey",
    "c_phone",
    "c_acctbal",
    "c_mktsegment",
    "c_comment",
];
const ORDERS_HEADERS: [&str; 9] = [
    "o_orderkey",
    "o_custkey",
    "o_orderstatus",
    "o_totalprice",
    "o_orderdate",
    "o_orderpriority",
    "o_clerk",
    "o_shippriority",
    "o_comment",
];
const LINEITEM_HEADERS: [&str; 16] = [
    "l_orderkey",
    "l_linenumber",
    "l_partkey",
    "l_suppkey",
    "l_quantity",
    "l_extendedprice",
    "l_discount",
    "l_tax",
    "l_returnflag",
    "l_linestatus",
    "l_shipdate",
    "l_commitdate",
    "l_receiptdate",
    "l_shipinstruct",
    "l_shipmode",
    "l_comment",
];

/// Options controlling TPC-H dataset loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadOptions {
    /// Maximum number of rows committed by each COPY batch.
    pub batch_rows: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            batch_rows: CopyOptions::DEFAULT_BATCH_ROWS,
        }
    }
}

/// Reports returned by loading the eight TPC-H tables in [`TABLE_NAMES`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// COPY reports in `region`, `nation`, `part`, `supplier`, `partsupp`,
    /// `customer`, `orders`, `lineitem` order.
    pub reports: [CopyReport; 8],
}

/// Formats a scaled DECIMAL(15,2) value without using floating point.
pub fn format_decimal(scaled_value: i64) -> String {
    let absolute = scaled_value.unsigned_abs();
    let sign = if scaled_value < 0 { "-" } else { "" };
    format!("{}{}.{:02}", sign, absolute / 100, absolute % 100)
}

pub(crate) fn format_quantity(quantity: i64) -> String {
    format!("{quantity}.00")
}

fn region_fields(row: &Region) -> io::Result<Vec<String>> {
    Ok(vec![
        row.r_regionkey.to_string(),
        row.r_name.clone(),
        row.r_comment.clone(),
    ])
}

fn nation_fields(row: &Nation) -> io::Result<Vec<String>> {
    Ok(vec![
        row.n_nationkey.to_string(),
        row.n_name.clone(),
        row.n_regionkey.to_string(),
        row.n_comment.clone(),
    ])
}

fn part_fields(row: &Part) -> io::Result<Vec<String>> {
    Ok(vec![
        row.p_partkey.to_string(),
        row.p_name.clone(),
        row.p_mfgr.clone(),
        row.p_brand.clone(),
        row.p_type.clone(),
        row.p_size.to_string(),
        row.p_container.clone(),
        format_decimal(row.p_retailprice),
        row.p_comment.clone(),
    ])
}

fn supplier_fields(row: &Supplier) -> io::Result<Vec<String>> {
    Ok(vec![
        row.s_suppkey.to_string(),
        row.s_name.clone(),
        row.s_address.clone(),
        row.s_nationkey.to_string(),
        row.s_phone.clone(),
        format_decimal(row.s_acctbal),
        row.s_comment.clone(),
    ])
}

fn partsupp_fields(row: &Partsupp) -> io::Result<Vec<String>> {
    Ok(vec![
        row.ps_partkey.to_string(),
        row.ps_suppkey.to_string(),
        row.ps_availqty.to_string(),
        format_decimal(row.ps_supplycost),
        row.ps_comment.clone(),
    ])
}

fn customer_fields(row: &Customer) -> io::Result<Vec<String>> {
    Ok(vec![
        row.c_custkey.to_string(),
        row.c_name.clone(),
        row.c_address.clone(),
        row.c_nationkey.to_string(),
        row.c_phone.clone(),
        format_decimal(row.c_acctbal),
        row.c_mktsegment.clone(),
        row.c_comment.clone(),
    ])
}

fn orders_fields(row: &Orders) -> io::Result<Vec<String>> {
    Ok(vec![
        row.o_orderkey.to_string(),
        row.o_custkey.to_string(),
        row.o_orderstatus.clone(),
        format_decimal(row.o_totalprice),
        row.o_orderdate.clone(),
        row.o_orderpriority.clone(),
        row.o_clerk.clone(),
        row.o_shippriority.to_string(),
        row.o_comment.clone(),
    ])
}

fn lineitem_fields(row: &Lineitem) -> io::Result<Vec<String>> {
    Ok(vec![
        row.l_orderkey.to_string(),
        row.l_linenumber.to_string(),
        row.l_partkey.to_string(),
        row.l_suppkey.to_string(),
        format_quantity(row.l_quantity),
        format_decimal(row.l_extendedprice),
        format_decimal(row.l_discount),
        format_decimal(row.l_tax),
        row.l_returnflag.clone(),
        row.l_linestatus.clone(),
        row.l_shipdate.clone(),
        row.l_commitdate.clone(),
        row.l_receiptdate.clone(),
        row.l_shipinstruct.clone(),
        row.l_shipmode.clone(),
        row.l_comment.clone(),
    ])
}

/// A pull-based CSV stream which only materializes approximately 64 KiB at a time.
struct CsvRowsReader<'a, T, F> {
    header: &'a [&'a str],
    rows: &'a [T],
    fields: F,
    next_row: usize,
    header_written: bool,
    bytes: Vec<u8>,
    position: usize,
    error: Option<io::Error>,
}

impl<'a, T, F> CsvRowsReader<'a, T, F>
where
    F: Fn(&T) -> io::Result<Vec<String>>,
{
    fn new(header: &'a [&'a str], rows: &'a [T], fields: F) -> Self {
        Self {
            header,
            rows,
            fields,
            next_row: 0,
            header_written: false,
            bytes: Vec::new(),
            position: 0,
            error: None,
        }
    }

    fn refill(&mut self) -> io::Result<()> {
        // `read` only requests a refill after consuming the current buffer.
        debug_assert_eq!(self.position, self.bytes.len());
        self.bytes.clear();
        self.position = 0;

        if self.next_row == self.rows.len() && self.header_written {
            return Ok(());
        }

        if !self.header_written {
            let mut encoded = Vec::new();
            {
                let mut writer = csv::WriterBuilder::new()
                    .has_headers(false)
                    .from_writer(&mut encoded);
                writer.write_record(self.header.iter().copied())?;
                writer.flush()?;
            }
            self.bytes.extend_from_slice(&encoded);
            self.header_written = true;
        }

        while self.next_row < self.rows.len() && self.bytes.len() < CSV_REFILL_BYTES {
            let fields = (self.fields)(&self.rows[self.next_row])?;
            let mut encoded = Vec::new();

            {
                let mut writer = csv::WriterBuilder::new()
                    .has_headers(false)
                    .from_writer(&mut encoded);
                writer.write_record(fields)?;
                writer.flush()?;
            }

            self.bytes.extend_from_slice(&encoded);
            // Advance only after the complete encoded record is in the refill buffer.
            self.next_row += 1;
        }

        Ok(())
    }
}

impl<T, F> Read for CsvRowsReader<'_, T, F>
where
    F: Fn(&T) -> io::Result<Vec<String>>,
{
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }

        loop {
            if self.position < self.bytes.len() {
                let available = &self.bytes[self.position..];
                let count = available.len().min(output.len());
                output[..count].copy_from_slice(&available[..count]);
                self.position += count;
                return Ok(count);
            }

            if let Some(error) = self.error.take() {
                return Err(error);
            }

            if self.next_row == self.rows.len() && self.header_written {
                return Ok(0);
            }

            if let Err(error) = self.refill() {
                self.error = Some(error);
            }
        }
    }
}

/// Loads a generated TPC-H dataset into a fresh local database.
pub fn load_dataset(
    server: &LocalServer,
    server_root: &Path,
    dataset: &Dataset,
    options: &LoadOptions,
) -> Result<LoadReport> {
    if options.batch_rows == 0 {
        return Err(HtapError::InvalidArgument(
            "batch_rows must be greater than 0".into(),
        ));
    }
    if options.batch_rows > CopyOptions::MAX_BATCH_ROWS {
        return Err(HtapError::InvalidArgument(format!(
            "batch_rows {} exceeds maximum limit {}",
            options.batch_rows,
            CopyOptions::MAX_BATCH_ROWS
        )));
    }

    let catalog = LocalCatalogStore::open(server_root.join("catalog"))?;
    if let Some(snapshot) = catalog.load()? {
        for table_name in TABLE_NAMES {
            if snapshot.table_by_name(table_name).is_some() {
                return Err(HtapError::InvalidArgument(format!(
                    "TPC-H load requires a fresh database; table '{table_name}' already exists"
                )));
            }
        }
    }

    for ddl in ddl_statements() {
        match server.execute(ddl)? {
            StatementResult::Command(CommandResult::Ddl { .. }) => {}
            result => {
                return Err(HtapError::InvalidArgument(format!(
                    "TPC-H DDL did not produce a DDL result: {result:?}"
                )));
            }
        }
    }

    let snapshot = catalog
        .load()?
        .ok_or_else(|| HtapError::InvalidArgument("TPC-H DDL did not create a catalog".into()))?;

    let mut table_ids = [None; 8];
    let mut tablet_ids = [None; 8];
    for (index, table_name) in TABLE_NAMES.iter().enumerate() {
        let table = snapshot.table_by_name(table_name).ok_or_else(|| {
            HtapError::InvalidArgument(format!("TPC-H DDL did not create table '{table_name}'"))
        })?;
        let partition_id = *table.partitions.first().ok_or_else(|| {
            HtapError::InvalidArgument(format!("TPC-H table '{table_name}' has no partition"))
        })?;
        let partition = snapshot.partition(partition_id).ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "TPC-H table '{table_name}' references a missing partition"
            ))
        })?;
        table_ids[index] = Some(table.id);
        tablet_ids[index] = Some(*partition.tablets.first().ok_or_else(|| {
            HtapError::InvalidArgument(format!("TPC-H table '{table_name}' has no tablet"))
        })?);
    }

    macro_rules! import_table {
        ($index:expr, $rows:expr, $headers:expr, $fields:expr) => {{
            let table_name = TABLE_NAMES[$index];
            let options = CopyOptions::new(
                format!("tpch-load-{table_name}"),
                table_ids[$index].expect("table ID resolved above"),
                tablet_ids[$index].expect("tablet ID resolved above"),
                DataFormat::Csv,
                server_root.join(format!(".tpch-load-{table_name}.csv")),
            )
            .with_batch_rows(options.batch_rows);
            let reader = CsvRowsReader::new(&$headers, $rows, $fields);
            server.copy_from_csv_reader(&options, reader)?
        }};
    }

    let reports = [
        import_table!(0, &dataset.region, REGION_HEADERS, region_fields),
        import_table!(1, &dataset.nation, NATION_HEADERS, nation_fields),
        import_table!(2, &dataset.part, PART_HEADERS, part_fields),
        import_table!(3, &dataset.supplier, SUPPLIER_HEADERS, supplier_fields),
        import_table!(4, &dataset.partsupp, PARTSUPP_HEADERS, partsupp_fields),
        import_table!(5, &dataset.customer, CUSTOMER_HEADERS, customer_fields),
        import_table!(6, &dataset.orders, ORDERS_HEADERS, orders_fields),
        import_table!(7, &dataset.lineitem, LINEITEM_HEADERS, lineitem_fields),
    ];

    let expected_lengths = [
        dataset.region.len(),
        dataset.nation.len(),
        dataset.part.len(),
        dataset.supplier.len(),
        dataset.partsupp.len(),
        dataset.customer.len(),
        dataset.orders.len(),
        dataset.lineitem.len(),
    ];

    for ((table_name, report), expected) in
        TABLE_NAMES.iter().zip(reports.iter()).zip(expected_lengths)
    {
        let expected = expected as u64;
        if report.records_read != expected
            || report.records_committed != expected
            || report.rows_written != expected
            || report.records_skipped != 0
        {
            return Err(HtapError::InvalidArgument(format!(
                "TPC-H load verification failed for '{table_name}': \
                 read={}, committed={}, written={}, skipped={}, expected={expected}",
                report.records_read,
                report.records_committed,
                report.rows_written,
                report.records_skipped,
            )));
        }
    }

    Ok(LoadReport { reports })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::{format_decimal, format_quantity, lineitem_fields, orders_fields, CsvRowsReader};

    #[test]
    fn csv_row_encoder() {
        let rows = [vec![
            "plain".to_string(),
            "with,comma".to_string(),
            "with \"quote\"".to_string(),
            "with\nnewline".to_string(),
        ]];
        let mut reader = CsvRowsReader::new(&["a", "b", "c", "d"], &rows, |row| Ok(row.clone()));
        let mut encoded = String::new();
        reader.read_to_string(&mut encoded).unwrap();

        assert_eq!(
            encoded,
            "a,b,c,d\nplain,\"with,comma\",\"with \"\"quote\"\"\",\"with\nnewline\"\n"
        );
    }

    #[test]
    fn decimal_and_date() {
        assert_eq!(format_decimal(12_345), "123.45");
        assert_eq!(format_decimal(-1_234), "-12.34");
        assert_eq!(format_decimal(i64::MIN), "-92233720368547758.08");
        assert_eq!(format_quantity(25), "25.00");

        let dataset = crate::generate::generate("0.01", 0).expect("generate test dataset");
        let order = dataset
            .orders
            .first()
            .expect("generated dataset contains orders");
        let lineitem = dataset
            .lineitem
            .first()
            .expect("generated dataset contains lineitems");

        let order_fields = orders_fields(order).expect("encode order fields");
        assert_eq!(order_fields[4], order.o_orderdate);

        let lineitem_fields = lineitem_fields(lineitem).expect("encode lineitem fields");
        assert_eq!(lineitem_fields[10], lineitem.l_shipdate);
        assert_eq!(lineitem_fields[11], lineitem.l_commitdate);
        assert_eq!(lineitem_fields[12], lineitem.l_receiptdate);
    }
}
