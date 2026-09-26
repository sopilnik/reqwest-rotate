//! Fetches one URL with rate limiting, retries and a backoff policy, no
//! proxies involved. Run with `cargo run --example get -- <url>`.

use std::time::Duration;

use reqwest_rotate::RotatingClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(url) = std::env::args().nth(1) else {
        println!("usage: get <url>");
        return Ok(());
    };

    let client = RotatingClient::builder()
        .rate_limit(Duration::from_millis(200))
        .retries(3)
        .backoff(Duration::from_millis(200), Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()?;

    let response = client.get(&url).await?;
    println!("status: {}", response.status());
    let body = response.text().await?;
    println!("{}", body.lines().next().unwrap_or(""));
    Ok(())
}
