//! Counts retries by reason using the `on_retry` hook, with no `tracing`
//! subscriber involved. Each event also carries the host the failed
//! attempt was sent to (`event.host`), left out of this table to keep it
//! short. Run with `cargo run --example retry_metrics -- <url>`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use reqwest_rotate::RotatingClient;

const RETRIES: u32 = 3;
const USAGE_EXIT_CODE: i32 = 2;
const LOCK_EXPECT: &str = "the retry hook never panics while it holds this lock";

/// Prints how many retries each reason caused, or that there were none.
fn print_counts(counts: &BTreeMap<&'static str, usize>) {
    if counts.is_empty() {
        println!("no retries");
    }

    for (reason, count) in counts {
        println!("{reason}: {count}");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(url) = std::env::args().nth(1) else {
        eprintln!("usage: retry_metrics <url>");
        std::process::exit(USAGE_EXIT_CODE);
    };

    let counts = Arc::new(Mutex::new(BTreeMap::<&'static str, usize>::new()));
    let counted = Arc::clone(&counts);
    let client = RotatingClient::builder()
        .retries(RETRIES)
        .on_retry(move |event| {
            *counted
                .lock()
                .expect(LOCK_EXPECT)
                .entry(event.reason.as_str())
                .or_insert(0) += 1;
        })
        .build()?;

    let result = client.get(&url).await;

    print_counts(&counts.lock().expect(LOCK_EXPECT));

    let response = result?;

    println!("status: {}", response.status());

    Ok(())
}
