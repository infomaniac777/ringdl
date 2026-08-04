mod engine;
mod http;
mod storage;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use engine::DownloadEngine;
use http::ParsedUrl;

/// High-performance HTTP downloader using io_uring Direct I/O and Provided Buffer Rings
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Target URL to download (HTTP)
    #[arg(required = true)]
    pub url: String,

    /// Output file path (defaults to filename from URL path if omitted)
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Block size in KiB for O_DIRECT aligned writes (default: 64 KiB)
    #[arg(long, default_value_t = 64)]
    pub block_size_kb: usize,

    /// Provided buffer ring size (number of entries, default: 128)
    #[arg(long, default_value_t = 128)]
    pub ring_entries: u16,

    /// Buffer size per entry in bytes (default: 131072 - 128 KiB)
    #[arg(long, default_value_t = 131072)]
    pub buf_size: usize,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    println!("⚡ ringdl v0.1.0 (io_uring Direct I/O Downloader)");
    println!("--------------------------------------------------");

    let parsed_url = ParsedUrl::parse(&args.url)?;
    
    let output_path = match args.output {
        Some(path) => path,
        None => {
            let path_segment = parsed_url.path.split('?').next().unwrap_or(&parsed_url.path);
            let filename = path_segment
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("downloaded_file");
            PathBuf::from(filename)
        }
    };

    println!("Target: {}", args.url);
    println!("Output: {:?}", output_path);
    println!("O_DIRECT Block Size: {} KiB", args.block_size_kb);
    println!("io_uring Buffer Ring Entries: {}", args.ring_entries);
    println!("--------------------------------------------------");

    let mut engine = DownloadEngine::new(args.ring_entries, args.buf_size, args.block_size_kb)?;
    engine.download(&parsed_url, &output_path)?;

    println!("✨ Finished successfully!");
    Ok(())
}
