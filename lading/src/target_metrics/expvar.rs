//! Expvar target metrics fetcher
//!
//! This module scrapes Go expvar formatted metrics from the target software.
//! The metrics are formatted as a JSON tree that is fetched over HTTP.

use std::time::Duration;

use metrics::gauge;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, info, trace};

use crate::signals::Shutdown;

#[derive(Debug, Clone, Copy, thiserror::Error)]
/// Errors produced by [`Expvar`]
pub enum Error {
    /// Expvar scraper shut down unexpectedly
    #[error("Unexpected shutdown")]
    EarlyShutdown,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Configuration for collecting Go Expvar based target metrics
pub struct Config {
    /// URI to read expvars from
    uri: String,
    /// Variable names to scrape
    vars: Vec<String>,
}

/// The `Expvar` target metrics implementation.
#[derive(Debug)]
pub struct Expvar {
    config: Config,
    shutdown: Shutdown,
}

impl Expvar {
    /// Create a new [`ExpVar`] instance
    ///
    /// This is responsible for scraping metrics from the target process
    /// using Go's expvar format.
    ///
    pub(crate) fn new(config: Config, shutdown: Shutdown) -> Self {
        Self { config, shutdown }
    }

    /// Run this [`Server`] to completion
    ///
    /// Scrape expvars from the target at 1Hz.
    ///
    /// # Errors
    ///
    /// None are known.
    ///
    /// # Panics
    ///
    /// None are known.
    pub(crate) async fn run(mut self) -> Result<(), Error> {
        info!("Expvar target metrics scraper running");
        let client = reqwest::Client::new();

        let server = async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                let Ok(resp) = client.get(&self.config.uri).timeout(Duration::from_secs(1)).send().await else {
                    info!("failed to get expvar uri");
                    continue;
                };

                let Ok(json) = resp.json::<Value>().await else {
                    info!("failed to parse expvar json");
                    continue;
                };

                for var_name in &self.config.vars {
                    let val = json.pointer(var_name).and_then(serde_json::Value::as_f64);
                    if let Some(val) = val {
                        trace!("expvar: {} = {}", var_name, val);
                        gauge!(format!("target/{name}", name = var_name.trim_start_matches('/')), val, "source" => "target_metrics/expvar");
                    }
                }
            }
        };

        tokio::select! {
            _res = server => {
                error!("server shutdown unexpectedly");
                 Err(Error::EarlyShutdown)
            }
            _ = self.shutdown.recv() => {
                info!("shutdown signal received");
                 Ok(())
            }
        }
    }
}
