//! Counts retries by reason using the `on_retry` hook, with no `tracing`
//! subscriber involved. Run with `cargo run --example retry_metrics -- <url>`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use reqwest_rotate::{RetryReason, RotatingClient};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(url) = std::env::args().nth(1) else {
        println!("usage: retry_metrics <url>");
        return Ok(());
    };

    let counts = Arc::new(Mutex::new(BTreeMap::<RetryReason, usize>::new()));
    let counted = Arc::clone(&counts);

    let client = RotatingClient::builder()
        .retries(3)
        .on_retry(move |event| {
            *counted.lock().unwrap().entry(event.reason).or_insert(0) += 1;
        })
        .build()?;

    let response = client.get(&url).await?;
    println!("status: {}", response.status());

    let counts = counts.lock().unwrap();
    if counts.is_empty() {
        println!("no retries");
    }
    for (reason, count) in counts.iter() {
        println!("{reason:?}: {count}");
    }
    Ok(())
}
