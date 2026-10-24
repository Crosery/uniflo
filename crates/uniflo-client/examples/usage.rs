//! Print token cost grouped by model, most expensive first.
//!
//! ```sh
//! cargo run -p uniflo-client --example usage
//! ```
//!
//! Needs a running `uniflo daemon`. Reads `UNIFLO_URL` / `UNIFLO_TOKEN` the same way the CLI
//! does.

use uniflo_client::{Client, UsageQuery};

fn main() -> Result<(), uniflo_client::Error> {
    let client = Client::local()?;
    let report = client.usage(&UsageQuery { group_by: Some("model".to_owned()), ..Default::default() })?;

    if !report.indexing.ready {
        eprintln!(
            "usage index is still catching up ({} / {} sources); totals will grow",
            report.indexing.done, report.indexing.total
        );
    }
    let mut rows = report.rows;
    rows.sort_by(|a, b| b.cost_usd.unwrap_or(0.0).total_cmp(&a.cost_usd.unwrap_or(0.0)));

    println!("{:<28} {:>8} {:>10} {:>10}", "model", "steps", "cost_usd", "unpriced");
    for row in &rows {
        let cost = row.cost_usd.map(|c| format!("{c:.2}")).unwrap_or_else(|| "-".to_owned());
        println!("{:<28} {:>8} {:>10} {:>10}", row.label, row.steps, cost, row.unpriced_steps);
    }
    let cost = report.totals.cost_usd.map(|c| format!("{c:.2}")).unwrap_or_else(|| "-".to_owned());
    println!("{:<28} {:>8} {:>10} {:>10}", "total", report.totals.steps, cost, report.totals.unpriced_steps);
    Ok(())
}
