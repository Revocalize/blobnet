use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use blobnet::provider::{self, Provider};
use blobnet::server::{self, Config};
use blobnet::statsd;
use clap::Parser;
use hyper::server::conn::AddrIncoming;
use hyperlocal::SocketIncoming;
use shadow_rs::shadow;
use shutdown::Shutdown;
use tikv_jemallocator::Jemalloc;

shadow!(build);

/// Low-latency, content-addressed file server with a non-volatile cache.
///
/// This file server can be configured to use one of multiple provider. Library
/// use is more flexible. For the command-line interface, it can read from an S3
/// bucket or local NFS-mounted directory, optionally with a fallback provider.
/// It also optionally takes a path to a cache directory.
///
/// Files are keyed by their content hashes, and the cache is meant to be
/// considered volatile at all times.
#[derive(Parser, Debug)]
#[clap(version, about, long_about = None, long_version = Some(build::CLAP_LONG_VERSION))]
pub struct Cli {
    /// String representation of the data provider.
    #[clap(short, long)]
    pub source: String,

    /// Fallback provider if data is not found in `source`.
    #[clap(short, long)]
    pub fallback: Option<String>,

    /// Cache directory for non-volatile local storage.
    #[clap(short, long)]
    pub cache: Option<PathBuf>,

    /// Secret used to authorize users to access the service.
    #[clap(long, env = "BLOBNET_SECRET")]
    pub secret: String,

    /// TCP port that the HTTP server listens on.
    #[clap(short, long, default_value_t = 7609)]
    pub port: u16,

    /// Listen on a Unix domain socket instead of `port`.
    #[clap(short, long)]
    pub unix_socket: Option<PathBuf>,

    /// Emit metrics to StatsD at 127.0.0.1:8125
    #[clap(long)]
    pub statsd: bool,
}

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// Attempt to parse a provider from CLI argument.
async fn parse_provider(source: &str) -> Result<Box<dyn Provider>> {
    let (kind, arg) = source
        .split_once(':')
        .with_context(|| format!("source {source:?} has no ':' character"))?;
    Ok(match kind {
        "memory" => Box::new(provider::Memory::new()),
        "s3" => {
            let sdk_config = aws_config::load_from_env().await;
            let s3 = aws_sdk_s3::Client::new(&sdk_config);
            Box::new(provider::S3::new(s3, arg).await?)
        }
        "localdir" => Box::new(provider::LocalDir::new(arg)),
        _ => bail!("unknown provider type {kind:?}"),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        tikv_jemalloc_ctl::background_thread::write(true).unwrap();
    }

    let args = Cli::parse();

    statsd::try_init(args.statsd)?;

    let mut provider = parse_provider(&args.source).await?;

    if let Some(fallback) = args.fallback {
        let fallback = parse_provider(&fallback).await?;
        provider = Box::new((provider, fallback));
    }

    if let Some(cache) = args.cache {
        // Server cache has 2 MiB page size.
        let caching = provider::Cached::new(provider, cache, 1 << 21);
        tokio::spawn(caching.cleaner());
        tokio::spawn(caching.stats_logger());
        tokio::spawn(caching.stats_emitter());
        provider = Box::new(caching);
    }

    let config = Config {
        provider,
        secret: args.secret,
    };

    if let Some(unix_socket) = args.unix_socket {
        let incoming = SocketIncoming::bind(&unix_socket)
            .with_context(|| format!("failed to bind to {unix_socket:?}"))?;
        let mut shutdown = Shutdown::new()?;
        server::listen_with_shutdown(config, incoming, shutdown.recv()).await?;
    } else {
        let addr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, args.port));
        let incoming = AddrIncoming::bind(&addr).context("failed to listen on address")?;
        let mut shutdown = Shutdown::new()?;
        server::listen_with_shutdown(config, incoming, shutdown.recv()).await?;
    }

    Ok(())
}
