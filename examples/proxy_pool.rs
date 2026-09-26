//! Rotates across proxies read from the `PROXIES` environment variable,
//! then reports each proxy's cooldown state. Run with:
//!
//!     PROXIES=http://proxy1:8080,http://proxy2:8080 cargo run --example proxy_pool -- <url>

use std::time::Duration;

use reqwest_rotate::RotatingClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let usage = "usage: PROXIES=<url,url,...> proxy_pool <url>";
    let Some(url) = std::env::args().nth(1) else {
        println!("{usage}");
        return Ok(());
    };
    let proxies: Vec<String> = std::env::var("PROXIES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|proxy| !proxy.is_empty())
        .map(str::to_owned)
        .collect();
    if proxies.is_empty() {
        println!("{usage}");
        return Ok(());
    }

    let client = RotatingClient::builder()
        .proxies(&proxies)
        .proxy_cooldown(Duration::from_secs(30))
        .retries(2)
        .build()?;

    let response = client.get(&url).await?;
    println!("status: {}", response.status());

    for proxy in client.proxies().iter() {
        // The list keeps credentials so it can connect; keep them off the screen.
        let mut shown = reqwest::Url::parse(proxy)?;
        let _ = shown.set_username("");
        let _ = shown.set_password(None);
        println!(
            "{shown} in_cooldown={}",
            client.proxies().in_cooldown(proxy)
        );
    }
    Ok(())
}
