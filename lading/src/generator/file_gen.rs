//! The file generator.
//!
//! Unlike the other generators the file generator does not "connect" however
//! losely to the target but instead, without coordination, merely writes files
//! on disk.
//!
//! ## Metrics
//!
//! `bytes_written`: Total bytes written
//! `bytes_per_second`: Configured rate to send data
//!
//! Additional metrics may be emitted by this generator's [throttle].
//!

use std::{
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
    str,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    thread,
};

use byte_unit::{Byte, ByteUnit};
use futures::future::join_all;
use lading_throttle::Throttle;
use metrics::{gauge, register_counter};
use rand::{prelude::StdRng, SeedableRng};
use serde::Deserialize;
use tokio::{
    fs,
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
    task::{JoinError, JoinHandle},
};
use tracing::info;

use crate::{
    block::{self, Block},
    common::PeekableReceiver,
    signals::Shutdown,
};

use super::General;

#[derive(thiserror::Error, Debug)]
/// Errors produced by [`FileGen`].
pub enum Error {
    /// Wrapper around [`std::io::Error`].
    #[error("Io error: {0}")]
    Io(#[from] ::std::io::Error),
    /// Creation of payload blocks failed.
    #[error("Block creation error: {0}")]
    Block(#[from] block::Error),
    /// Child sub-task error.
    #[error("Child join error: {0}")]
    Child(#[from] JoinError),
}

fn default_rotation() -> bool {
    true
}

#[derive(Debug, Deserialize, PartialEq)]
/// Configuration of [`FileGen`]
pub struct Config {
    /// The seed for random operations against this target
    pub seed: [u8; 32],
    /// The path template for logs. "%NNN%" will be replaced in the template
    /// with the duplicate number.
    pub path_template: String,
    /// Total number of duplicates to make from this template.
    pub duplicates: u8,
    /// Sets the [`crate::payload::Config`] of this template.
    pub variant: lading_payload::Config,
    /// Sets the **soft** maximum bytes to be written into the `LogTarget`. This
    /// limit is soft, meaning a burst may go beyond this limit by no more than
    /// `maximum_token_burst`.
    ///
    /// After this limit is breached the target is closed and deleted. A new
    /// target with the same name is created to be written to.
    maximum_bytes_per_file: Byte,
    /// Defines the number of bytes that are added into the `LogTarget`'s rate
    /// limiting mechanism per second. This sets the maximum bytes that can be
    /// written _continuously_ per second from this target. Higher bursts are
    /// possible as the internal governor accumulates, up to
    /// `maximum_bytes_burst`.
    bytes_per_second: Byte,
    /// The block sizes for messages to this target
    pub block_sizes: Option<Vec<byte_unit::Byte>>,
    /// Defines the maximum internal cache of this log target. file_gen will
    /// pre-build its outputs up to the byte capacity specified here.
    maximum_prebuild_cache_size_bytes: Byte,
    /// Whether to use a fixed or streaming block cache
    #[serde(default = "crate::block::default_cache_method")]
    block_cache_method: block::CacheMethod,
    /// Determines whether the file generator mimics log rotation or not. If
    /// true, files will be rotated. If false, it is the responsibility of
    /// tailing software to remove old files.
    #[serde(default = "default_rotation")]
    rotate: bool,
    /// The load throttle configuration
    #[serde(default)]
    pub throttle: lading_throttle::Config,
}

#[derive(Debug)]
/// The file generator.
///
/// This generator writes files to disk, rotating them as appropriate. It does
/// this without coordination to the target.
pub struct FileGen {
    handles: Vec<JoinHandle<Result<(), Error>>>,
    shutdown: Shutdown,
}

impl FileGen {
    /// Create a new [`FileGen`]
    ///
    /// # Errors
    ///
    /// Creation will fail if the target file cannot be opened for writing.
    ///
    /// # Panics
    ///
    /// Function will panic if variant is Static and the `static_path` is not
    /// set.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(general: General, config: Config, shutdown: Shutdown) -> Result<Self, Error> {
        let mut rng = StdRng::from_seed(config.seed);
        let block_sizes: Vec<NonZeroUsize> = config
            .block_sizes
            .unwrap_or_else(|| {
                vec![
                    Byte::from_unit(1_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(2_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(4_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(8_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(16_f64, ByteUnit::MB).unwrap(),
                    Byte::from_unit(32_f64, ByteUnit::MB).unwrap(),
                ]
            })
            .iter()
            .map(|sz| NonZeroUsize::new(sz.get_bytes() as usize).expect("bytes must be non-zero"))
            .collect();
        let mut labels = vec![
            ("component".to_string(), "generator".to_string()),
            ("component_name".to_string(), "file_gen".to_string()),
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

        let maximum_bytes_per_file =
            NonZeroU32::new(config.maximum_bytes_per_file.get_bytes() as u32).unwrap();

        let mut handles = Vec::new();
        let file_index = Arc::new(AtomicU32::new(0));

        for _ in 0..config.duplicates {
            let throttle = Throttle::new_with_config(config.throttle, bytes_per_second);

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

            let child = Child {
                path_template: config.path_template.clone(),
                maximum_bytes_per_file,
                bytes_per_second,
                throttle,
                block_cache,
                file_index: Arc::clone(&file_index),
                rotate: config.rotate,
                shutdown: shutdown.clone(),
            };

            handles.push(tokio::spawn(child.spin()));
        }

        Ok(Self { handles, shutdown })
    }

    /// Run [`FileGen`] to completion or until a shutdown signal is received.
    ///
    /// In this loop the target file will be populated with lines of the variant
    /// dictated by the end user.
    ///
    /// # Errors
    ///
    /// This function will terminate with an error if file permissions are not
    /// correct, if the file cannot be written to etc. Any error from
    /// `std::io::Error` is possible.
    #[allow(clippy::cast_precision_loss)]
    #[allow(clippy::cast_possible_truncation)]
    pub async fn spin(mut self) -> Result<(), Error> {
        self.shutdown.recv().await;
        info!("shutdown signal received");
        for res in join_all(self.handles.drain(..)).await {
            match res {
                Ok(Ok(())) => continue,
                Ok(Err(err)) => return Err(err),
                Err(err) => return Err(Error::Child(err)),
            }
        }
        Ok(())
    }
}

struct Child {
    path_template: String,
    maximum_bytes_per_file: NonZeroU32,
    bytes_per_second: NonZeroU32,
    throttle: Throttle,
    block_cache: block::Cache,
    rotate: bool,
    file_index: Arc<AtomicU32>,
    shutdown: Shutdown,
}

impl Child {
    pub(crate) async fn spin(mut self) -> Result<(), Error> {
        let bytes_per_second = self.bytes_per_second.get() as usize;
        let mut total_bytes_written: u64 = 0;
        let maximum_bytes_per_file: u64 = u64::from(self.maximum_bytes_per_file.get());

        let mut file_index = self.file_index.fetch_add(1, Ordering::Relaxed);
        let mut path = path_from_template(&self.path_template, file_index);

        let mut fp = BufWriter::with_capacity(
            bytes_per_second,
            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .await?,
        );

        // Move the block_cache into an OS thread, exposing a channel between it
        // and this async context.
        let block_cache = self.block_cache;
        let (snd, rcv) = mpsc::channel(1024);
        let mut rcv: PeekableReceiver<Block> = PeekableReceiver::new(rcv);
        thread::Builder::new().spawn(|| block_cache.spin(snd))?;
        let bytes_written = register_counter!("bytes_written");

        loop {
            let blk = rcv.peek().await.unwrap();
            let total_bytes = blk.total_bytes;

            tokio::select! {
                _ = self.throttle.wait_for(total_bytes) => {
                    let blk = rcv.next().await.unwrap(); // actually advance through the blocks
                    let total_bytes = u64::from(total_bytes.get());

                    {
                        fp.write_all(&blk.bytes).await?;
                        bytes_written.increment(total_bytes);
                        total_bytes_written += total_bytes;
                    }

                    if total_bytes_written > maximum_bytes_per_file {
                        fp.flush().await?;
                        if self.rotate {
                            // Delete file, leaving any open file handlers intact. This
                            // includes our own `fp` for the time being.
                            fs::remove_file(&path).await?;
                        }
                        // Update `path` to point to the next indexed file.
                        file_index = self.file_index.fetch_add(1, Ordering::Relaxed);
                        path = path_from_template(&self.path_template, file_index);
                        // Open a new fp to `path`, replacing `fp`. Any holders of the
                        // file pointer still have it but the file no longer has a name.
                        fp = BufWriter::with_capacity(
                            bytes_per_second,
                            fs::OpenOptions::new()
                                .create(true)
                                .truncate(false)
                                .write(true)
                                .open(&path)
                                .await?,
                        );
                        total_bytes_written = 0;
                    }
                }
                _ = self.shutdown.recv() => {
                    fp.flush().await?;
                    info!("shutdown signal received");
                    return Ok(());
                },
            }
        }
    }
}

#[inline]
fn path_from_template(path_template: &str, index: u32) -> PathBuf {
    let fidx = format!("{index:04}");
    let full_path = path_template.replace("%NNN%", &fidx);
    PathBuf::from(full_path)
}
