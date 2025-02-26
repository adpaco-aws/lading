//! The Unix Domain Socket stream speaking generator.
//!
//! ## Metrics
//!
//! `bytes_written`: Bytes sent successfully
//! `packets_sent`: Packets sent successfully
//! `request_failure`: Number of failed writes; each occurrence causes a reconnect
//! `connection_failure`: Number of connection failures
//! `bytes_per_second`: Configured rate to send data
//!
//! Additional metrics may be emitted by this generator's [throttle].
//!

use crate::{
    block::{self, Block},
    common::PeekableReceiver,
    signals::Shutdown,
};
use byte_unit::{Byte, ByteUnit};
use lading_throttle::Throttle;
use metrics::{counter, gauge, register_counter};
use rand::{rngs::StdRng, SeedableRng};
use serde::Deserialize;
use std::{
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    thread,
};
use tokio::{net, sync::mpsc, task::JoinError};
use tracing::{debug, error, info};

use super::General;

#[derive(Debug, Deserialize, PartialEq)]
/// Configuration of this generator.
pub struct Config {
    /// The seed for random operations against this target
    pub seed: [u8; 32],
    /// The path of the socket to write to.
    pub path: PathBuf,
    /// The payload variant
    pub variant: lading_payload::Config,
    /// The bytes per second to send or receive from the target
    pub bytes_per_second: byte_unit::Byte,
    /// The block sizes for messages to this target
    pub block_sizes: Option<Vec<byte_unit::Byte>>,
    /// The maximum size in bytes of the cache of prebuilt messages
    pub maximum_prebuild_cache_size_bytes: byte_unit::Byte,
    /// Whether to use a fixed or streaming block cache
    #[serde(default = "crate::block::default_cache_method")]
    pub block_cache_method: block::CacheMethod,
    /// The load throttle configuration
    #[serde(default)]
    pub throttle: lading_throttle::Config,
}

/// Errors produced by [`UnixStream`].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Creation of payload blocks failed.
    #[error("Creation of payload blocks failed: {0}")]
    Block(#[from] block::Error),
    /// Generic IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Subtask error
    #[error("Subtask failure: {0}")]
    Subtask(#[from] JoinError),
}

#[derive(Debug)]
/// The Unix Domain Socket stream generator.
///
/// This generator is responsible for sending data to the target via UDS
/// streams.
pub struct UnixStream {
    path: PathBuf,
    throttle: Throttle,
    block_cache: block::Cache,
    metric_labels: Vec<(String, String)>,
    shutdown: Shutdown,
}

impl UnixStream {
    /// Create a new [`UnixStream`] instance
    ///
    /// # Errors
    ///
    /// Creation will fail if the underlying governor capacity exceeds u32.
    ///
    /// # Panics
    ///
    /// Function will panic if user has passed zero values for any byte
    /// values. Sharp corners.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(general: General, config: Config, shutdown: Shutdown) -> Result<Self, Error> {
        let mut rng = StdRng::from_seed(config.seed);
        let block_sizes: Vec<NonZeroUsize> = config
            .block_sizes
            .clone()
            .unwrap_or_else(|| {
                vec![
                    Byte::from_unit(1.0 / 32.0, ByteUnit::MB).unwrap(),
                    Byte::from_unit(1.0 / 16.0, ByteUnit::MB).unwrap(),
                    Byte::from_unit(1.0 / 8.0, ByteUnit::MB).unwrap(),
                    Byte::from_unit(1.0 / 4.0, ByteUnit::MB).unwrap(),
                    Byte::from_unit(1.0 / 2.0, ByteUnit::MB).unwrap(),
                    Byte::from_unit(1_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(2_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(4_f64, ByteUnit::MB).unwrap(),
                ]
            })
            .iter()
            .map(|sz| NonZeroUsize::new(sz.get_bytes() as usize).expect("bytes must be non-zero"))
            .collect();
        let mut labels = vec![
            ("component".to_string(), "generator".to_string()),
            ("component_name".to_string(), "unix_stream".to_string()),
        ];
        if let Some(id) = general.id {
            labels.push(("id".to_string(), id));
        }

        let bytes_per_second = NonZeroU32::new(config.bytes_per_second.get_bytes() as u32).unwrap();
        gauge!(
            "bytes_per_second",
            f64::from(bytes_per_second.get()),
            &labels
        );

        let total_bytes =
            NonZeroUsize::new(config.maximum_prebuild_cache_size_bytes.get_bytes() as usize)
                .expect("bytes must be non-zero");
        let block_cache = match config.block_cache_method {
            block::CacheMethod::Streaming => block::Cache::stream(
                config.seed,
                total_bytes,
                &block_sizes,
                config.variant.clone(),
            )?,
            block::CacheMethod::Fixed => {
                block::Cache::fixed(&mut rng, total_bytes, &block_sizes, &config.variant)?
            }
        };

        Ok(Self {
            path: config.path,
            block_cache,
            throttle: Throttle::new_with_config(config.throttle, bytes_per_second),
            metric_labels: labels,
            shutdown,
        })
    }

    /// Run [`UnixStream`] to completion or until a shutdown signal is received.
    ///
    /// # Errors
    ///
    /// Function will return an error when the UDS socket cannot be written to.
    ///
    /// # Panics
    ///
    /// Function will panic if underlying byte capacity is not available.
    pub async fn spin(mut self) -> Result<(), Error> {
        debug!("UnixStream generator running");

        // Move the block_cache into an OS thread, exposing a channel between it
        // and this async context.
        let block_cache = self.block_cache;
        let (snd, rcv) = mpsc::channel(1024);
        let mut rcv: PeekableReceiver<Block> = PeekableReceiver::new(rcv);
        thread::Builder::new().spawn(|| block_cache.spin(snd))?;
        let mut unix_stream = Option::<net::UnixStream>::None;

        let bytes_written = register_counter!("bytes_written", &self.metric_labels);
        let packets_sent = register_counter!("packets_sent", &self.metric_labels);

        loop {
            let blk = rcv.peek().await.unwrap();
            let total_bytes = blk.total_bytes;

            tokio::select! {
                sock = net::UnixStream::connect(&self.path), if unix_stream.is_none() => {
                    match sock {
                        Ok(stream) => {
                            debug!("UDS socket opened for writing.");
                            unix_stream = Some(stream);
                        }
                        Err(err) => {
                            error!("Opening UDS path failed: {}", err);

                            let mut error_labels = self.metric_labels.clone();
                            error_labels.push(("error".to_string(), err.to_string()));
                            counter!("connection_failure", 1, &error_labels);
                        }
                    }
                }
                _ = self.throttle.wait_for(total_bytes), if unix_stream.is_some() => {
                    // NOTE When we write into a unix stream it may be that only
                    // some of the written bytes make it through in which case we
                    // must cycle back around and try to write the remainder of the
                    // buffer.
                    let blk_max: usize = total_bytes.get() as usize;
                    let mut blk_offset = 0;
                    let blk = rcv.next().await.unwrap(); // advance to the block that was previously peeked
                    while blk_offset < blk_max {
                        let stream = unix_stream.unwrap();
                        unix_stream = None;

                        let ready = stream
                            .ready(tokio::io::Interest::WRITABLE)
                            .await
                            .map_err(Error::Io)
                            .unwrap(); // Cannot ? in a spawned task :<. Mimics UDP generator.
                        if ready.is_writable() {
                            // Try to write data, this may still fail with `WouldBlock`
                            // if the readiness event is a false positive.
                            match stream.try_write(&blk.bytes[blk_offset..]) {
                                Ok(bytes) => {
                                    bytes_written.increment(bytes as u64);
                                    packets_sent.increment(1);
                                    blk_offset = bytes;
                                }
                                Err(ref e) if e.kind() == tokio::io::ErrorKind::WouldBlock => {
                                    // If the read side has hung up we will never
                                    // know and will keep attempting to write into
                                    // the stream. This yield means we won't hog the
                                    // whole CPU.
                                    tokio::task::yield_now().await;
                                }
                                Err(err) => {
                                    debug!("write failed: {}", err);

                                    let mut error_labels = self.metric_labels.clone();
                                    error_labels.push(("error".to_string(), err.to_string()));
                                    counter!("request_failure", 1, &error_labels);
                                    // NOTE we here skip replacing `stream` into
                                    // `unix_stream` and will attempt a new
                                    // connection.
                                    break;
                                }
                            }
                        }
                        unix_stream = Some(stream);
                    }
                }
                _ = self.shutdown.recv() => {
                    info!("shutdown signal received");
                    return Ok(());
                },
            }
        }
    }
}
