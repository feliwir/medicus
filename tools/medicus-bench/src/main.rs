//! `medicus-bench`: measures ingest throughput and latency through the real pipeline (built-in
//! storage and index, events), with synthetic instances generated up front.
//!
//! ```sh
//! cargo run --release -p medicus-bench -- --instances 2000 --pixel-bytes 524288 --concurrency 16
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use futures::StreamExt as _;
use medicus_core::event::Origin;
use medicus_server::{Config, Node};
use medicus_testkit::dicom::InstanceBuilder;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Ingest benchmark for medicus")]
struct Args {
    /// Number of instances to ingest.
    #[arg(long, default_value_t = 1000)]
    instances: usize,
    /// Pixel data size per instance in bytes.
    #[arg(long, default_value_t = 512 * 1024)]
    pixel_bytes: usize,
    /// Instances ingested concurrently.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    /// Instances per series (series per study is fixed at 4).
    #[arg(long, default_value_t = 100)]
    series_size: usize,
    /// Chunk size of the simulated incoming streams.
    #[arg(long, default_value_t = 64 * 1024)]
    chunk_bytes: usize,
    /// Data directory; a temporary one if not given.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Report {
    instances: usize,
    bytes: u64,
    concurrency: usize,
    seconds: f64,
    instances_per_second: f64,
    megabytes_per_second: f64,
    latency_ms: Latency,
}

#[derive(Debug, Serialize)]
struct Latency {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentile(sorted: &[Duration], p: usize) -> f64 {
    let i = (sorted.len() * p / 100).min(sorted.len().saturating_sub(1));
    sorted.get(i).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

#[allow(clippy::cast_precision_loss)] // reporting
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let tmp = tempfile::tempdir()?;
    let mut cfg = Config::default();
    cfg.server.data_dir = args
        .data_dir
        .clone()
        .unwrap_or_else(|| tmp.path().to_owned());
    let node = Node::open(cfg).await?;

    eprintln!(
        "generating {} instances of {} KiB pixel data ...",
        args.instances,
        args.pixel_bytes / 1024
    );
    let files: Vec<Vec<u8>> = (0..args.instances)
        .map(|i| {
            let series = i / args.series_size.max(1);
            let study = series / 4;
            InstanceBuilder::new(
                &format!("2.25.{study}"),
                &format!("2.25.{study}.{series}"),
                &format!("2.25.{study}.{series}.{i}"),
            )
            .pixel_data(args.pixel_bytes)
            .build()
        })
        .collect();
    let bytes: u64 = files.iter().map(|f| f.len() as u64).sum();

    eprintln!("ingesting with concurrency {} ...", args.concurrency);
    let archive = node.archive.clone();
    let chunk = args.chunk_bytes;
    let start = Instant::now();
    let mut latencies: Vec<Duration> = futures::stream::iter(files)
        .map(|file| {
            let archive = archive.clone();
            async move {
                let t = Instant::now();
                archive
                    .ingest(
                        medicus_testkit::stream(&file, chunk),
                        Origin::Internal {
                            component: "bench".into(),
                        },
                    )
                    .await
                    .map(|_| t.elapsed())
            }
        })
        .buffer_unordered(args.concurrency.max(1))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
    let seconds = start.elapsed().as_secs_f64();
    latencies.sort_unstable();

    let report = Report {
        instances: args.instances,
        bytes,
        concurrency: args.concurrency,
        seconds,
        instances_per_second: args.instances as f64 / seconds,
        megabytes_per_second: bytes as f64 / 1e6 / seconds,
        latency_ms: Latency {
            p50: percentile(&latencies, 50),
            p95: percentile(&latencies, 95),
            p99: percentile(&latencies, 99),
            max: latencies.last().map_or(0.0, |d| d.as_secs_f64() * 1000.0),
        },
    };
    #[allow(clippy::print_stdout)] // the tool's output
    {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}
