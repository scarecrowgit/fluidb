use std::io::{self, Read};
use std::path::Path;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::{HtapError, Result};
use htap_movement::{CopyOptions, CopyReport, DataFormat};
use htap_server::LocalServer;
use htap_sql::result::{CommandResult, StatementResult};

use crate::generate::{
    Customer, Dataset, District, History, Item, NewOrder, OrderLine, Orders, Stock, Warehouse,
};
use crate::schema::{ddl_statements, TABLE_NAMES};

const CSV_REFILL_BYTES: usize = 64 * 1024;
const NULL: &str = r"\N";

const WAREHOUSE_HEADERS: [&str; 9] = [
    "w_id",
    "w_name",
    "w_street_1",
    "w_street_2",
    "w_city",
    "w_state",
    "w_zip",
    "w_tax",
    "w_ytd",
];
const DISTRICT_HEADERS: [&str; 11] = [
    "d_id",
    "d_w_id",
    "d_name",
    "d_street_1",
    "d_street_2",
    "d_city",
    "d_state",
    "d_zip",
    "d_tax",
    "d_ytd",
    "d_next_o_id",
];
const ITEM_HEADERS: [&str; 5] = ["i_id", "i_im_id", "i_name", "i_price", "i_data"];
const STOCK_HEADERS: [&str; 17] = [
    "s_i_id",
    "s_w_id",
    "s_quantity",
    "s_dist_01",
    "s_dist_02",
    "s_dist_03",
    "s_dist_04",
    "s_dist_05",
    "s_dist_06",
    "s_dist_07",
    "s_dist_08",
    "s_dist_09",
    "s_dist_10",
    "s_ytd",
    "s_order_cnt",
    "s_remote_cnt",
    "s_data",
];
const CUSTOMER_HEADERS: [&str; 21] = [
    "c_id",
    "c_d_id",
    "c_w_id",
    "c_first",
    "c_middle",
    "c_last",
    "c_street_1",
    "c_street_2",
    "c_city",
    "c_state",
    "c_zip",
    "c_phone",
    "c_since",
    "c_credit",
    "c_credit_lim",
    "c_discount",
    "c_balance",
    "c_ytd_payment",
    "c_payment_cnt",
    "c_delivery_cnt",
    "c_data",
];
const HISTORY_HEADERS: [&str; 9] = [
    "h_id", "h_c_id", "h_c_d_id", "h_c_w_id", "h_d_id", "h_w_id", "h_date", "h_amount", "h_data",
];
const ORDERS_HEADERS: [&str; 8] = [
    "o_id",
    "o_d_id",
    "o_w_id",
    "o_c_id",
    "o_entry_d",
    "o_carrier_id",
    "o_ol_cnt",
    "o_all_local",
];
const ORDER_LINE_HEADERS: [&str; 10] = [
    "ol_o_id",
    "ol_d_id",
    "ol_w_id",
    "ol_number",
    "ol_i_id",
    "ol_supply_w_id",
    "ol_delivery_d",
    "ol_quantity",
    "ol_amount",
    "ol_dist_info",
];
const NEW_ORDER_HEADERS: [&str; 3] = ["no_o_id", "no_d_id", "no_w_id"];

/// Options controlling TPC-C dataset loading.
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

/// Reports returned by loading the nine TPC-C tables in [`TABLE_NAMES`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    /// COPY reports in schema table order.
    pub reports: [CopyReport; 9],
}

/// Formats a scaled integer decimal value with the requested fractional scale.
fn format_decimal_with_scale(value: i64, scale: u32) -> String {
    let absolute = value.unsigned_abs();
    let divisor = 10_u64.pow(scale);
    let sign = if value < 0 { "-" } else { "" };

    format!(
        "{}{}.{:0width$}",
        sign,
        absolute / divisor,
        absolute % divisor,
        width = scale as usize,
    )
}

/// Formats a scaled integer monetary decimal value with two fractional digits.
pub fn format_decimal(value: i64) -> String {
    format_decimal_with_scale(value, 2)
}

fn decimal(value: i64) -> String {
    format_decimal(value)
}

fn decimal_scale_4(value: i64) -> String {
    format_decimal_with_scale(value, 4)
}

fn warehouse_fields(row: &Warehouse) -> io::Result<Vec<String>> {
    Ok(vec![
        row.w_id.to_string(),
        row.w_name.clone(),
        row.w_street_1.clone(),
        row.w_street_2.clone(),
        row.w_city.clone(),
        row.w_state.clone(),
        row.w_zip.clone(),
        decimal_scale_4(row.w_tax),
        decimal(row.w_ytd),
    ])
}

fn district_fields(row: &District) -> io::Result<Vec<String>> {
    Ok(vec![
        row.d_id.to_string(),
        row.d_w_id.to_string(),
        row.d_name.clone(),
        row.d_street_1.clone(),
        row.d_street_2.clone(),
        row.d_city.clone(),
        row.d_state.clone(),
        row.d_zip.clone(),
        decimal_scale_4(row.d_tax),
        decimal(row.d_ytd),
        row.d_next_o_id.to_string(),
    ])
}

fn item_fields(row: &Item) -> io::Result<Vec<String>> {
    Ok(vec![
        row.i_id.to_string(),
        row.i_im_id.to_string(),
        row.i_name.clone(),
        decimal(row.i_price),
        row.i_data.clone(),
    ])
}

fn stock_fields(row: &Stock) -> io::Result<Vec<String>> {
    Ok(vec![
        row.s_i_id.to_string(),
        row.s_w_id.to_string(),
        row.s_quantity.to_string(),
        row.s_dist_01.clone(),
        row.s_dist_02.clone(),
        row.s_dist_03.clone(),
        row.s_dist_04.clone(),
        row.s_dist_05.clone(),
        row.s_dist_06.clone(),
        row.s_dist_07.clone(),
        row.s_dist_08.clone(),
        row.s_dist_09.clone(),
        row.s_dist_10.clone(),
        row.s_ytd.to_string(),
        row.s_order_cnt.to_string(),
        row.s_remote_cnt.to_string(),
        row.s_data.clone(),
    ])
}

fn customer_fields(row: &Customer) -> io::Result<Vec<String>> {
    Ok(vec![
        row.c_id.to_string(),
        row.c_d_id.to_string(),
        row.c_w_id.to_string(),
        row.c_first.clone(),
        row.c_middle.clone(),
        row.c_last.clone(),
        row.c_street_1.clone(),
        row.c_street_2.clone(),
        row.c_city.clone(),
        row.c_state.clone(),
        row.c_zip.clone(),
        row.c_phone.clone(),
        row.c_since.clone(),
        row.c_credit.clone(),
        decimal(row.c_credit_lim),
        decimal_scale_4(row.c_discount),
        decimal(row.c_balance),
        decimal(row.c_ytd_payment),
        row.c_payment_cnt.to_string(),
        row.c_delivery_cnt.to_string(),
        row.c_data.clone(),
    ])
}

fn history_fields(row: &History) -> io::Result<Vec<String>> {
    Ok(vec![
        row.h_id.to_string(),
        row.h_c_id.to_string(),
        row.h_c_d_id.to_string(),
        row.h_c_w_id.to_string(),
        row.h_d_id.to_string(),
        row.h_w_id.to_string(),
        row.h_date.clone(),
        decimal(row.h_amount),
        row.h_data.clone(),
    ])
}

fn orders_fields(row: &Orders) -> io::Result<Vec<String>> {
    Ok(vec![
        row.o_id.to_string(),
        row.o_d_id.to_string(),
        row.o_w_id.to_string(),
        row.o_c_id.to_string(),
        row.o_entry_d.clone(),
        row.o_carrier_id
            .map_or_else(|| NULL.to_owned(), |value| value.to_string()),
        row.o_ol_cnt.to_string(),
        row.o_all_local.to_string(),
    ])
}

fn order_line_fields(row: &OrderLine) -> io::Result<Vec<String>> {
    Ok(vec![
        row.ol_o_id.to_string(),
        row.ol_d_id.to_string(),
        row.ol_w_id.to_string(),
        row.ol_number.to_string(),
        row.ol_i_id.to_string(),
        row.ol_supply_w_id.to_string(),
        row.ol_delivery_d.clone().unwrap_or_else(|| NULL.to_owned()),
        row.ol_quantity.to_string(),
        decimal(row.ol_amount),
        row.ol_dist_info.clone(),
    ])
}

fn new_order_fields(row: &NewOrder) -> io::Result<Vec<String>> {
    Ok(vec![
        row.no_o_id.to_string(),
        row.no_d_id.to_string(),
        row.no_w_id.to_string(),
    ])
}

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
        self.bytes.clear();
        self.position = 0;
        if self.next_row == self.rows.len() && self.header_written {
            return Ok(());
        }

        if !self.header_written {
            let mut writer = csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(&mut self.bytes);
            writer.write_record(self.header.iter().copied())?;
            writer.flush()?;
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
                let count = (self.bytes.len() - self.position).min(output.len());
                output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
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

/// Loads a generated TPC-C dataset into a fresh local database.
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
                    "TPC-C load requires a fresh database; table '{table_name}' already exists"
                )));
            }
        }
    }

    for ddl in ddl_statements() {
        match server.execute(&ddl)? {
            StatementResult::Command(CommandResult::Ddl { .. }) => {}
            result => {
                return Err(HtapError::InvalidArgument(format!(
                    "TPC-C DDL did not produce a DDL result: {result:?}"
                )));
            }
        }
    }

    let snapshot = catalog
        .load()?
        .ok_or_else(|| HtapError::InvalidArgument("TPC-C DDL did not create a catalog".into()))?;
    let mut table_ids = [None; 9];
    let mut tablet_ids = [None; 9];
    for (index, table_name) in TABLE_NAMES.iter().enumerate() {
        let table = snapshot.table_by_name(table_name).ok_or_else(|| {
            HtapError::InvalidArgument(format!("TPC-C DDL did not create table '{table_name}'"))
        })?;
        let partition_id = *table.partitions.first().ok_or_else(|| {
            HtapError::InvalidArgument(format!("TPC-C table '{table_name}' has no partition"))
        })?;
        let partition = snapshot.partition(partition_id).ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "TPC-C table '{table_name}' references a missing partition"
            ))
        })?;
        table_ids[index] = Some(table.id);
        tablet_ids[index] = Some(*partition.tablets.first().ok_or_else(|| {
            HtapError::InvalidArgument(format!("TPC-C table '{table_name}' has no tablet"))
        })?);
    }

    macro_rules! import_table {
        ($index:expr, $rows:expr, $headers:expr, $fields:expr) => {{
            let table_name = TABLE_NAMES[$index];
            let copy_options = CopyOptions::new(
                format!("tpcc-load-{table_name}"),
                table_ids[$index].expect("table ID resolved above"),
                tablet_ids[$index].expect("tablet ID resolved above"),
                DataFormat::Csv,
                server_root.join(format!(".tpcc-load-{table_name}.csv")),
            )
            .with_batch_rows(options.batch_rows);
            server.copy_from_csv_reader(
                &copy_options,
                CsvRowsReader::new(&$headers, $rows, $fields),
            )?
        }};
    }

    let reports = [
        import_table!(0, &dataset.warehouse, WAREHOUSE_HEADERS, warehouse_fields),
        import_table!(1, &dataset.district, DISTRICT_HEADERS, district_fields),
        import_table!(2, &dataset.item, ITEM_HEADERS, item_fields),
        import_table!(3, &dataset.stock, STOCK_HEADERS, stock_fields),
        import_table!(4, &dataset.customer, CUSTOMER_HEADERS, customer_fields),
        import_table!(5, &dataset.history, HISTORY_HEADERS, history_fields),
        import_table!(6, &dataset.orders, ORDERS_HEADERS, orders_fields),
        import_table!(
            7,
            &dataset.order_line,
            ORDER_LINE_HEADERS,
            order_line_fields
        ),
        import_table!(8, &dataset.new_order, NEW_ORDER_HEADERS, new_order_fields),
    ];
    let expected = [
        dataset.warehouse.len(),
        dataset.district.len(),
        dataset.item.len(),
        dataset.stock.len(),
        dataset.customer.len(),
        dataset.history.len(),
        dataset.orders.len(),
        dataset.order_line.len(),
        dataset.new_order.len(),
    ];

    for ((table_name, report), expected) in TABLE_NAMES.iter().zip(reports.iter()).zip(expected) {
        let expected = expected as u64;
        if report.records_read != expected
            || report.records_committed != expected
            || report.rows_written != expected
            || report.records_skipped != 0
        {
            return Err(HtapError::InvalidArgument(format!(
                "TPC-C load verification failed for '{table_name}': \
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
