//! BuildKit reporting agent: sidecar that talks to BuildKit over gRPC (Control API only)
//! and exposes metrics for builds, cache, and workers.

mod generated;
mod metrics;

use generated::{
    control_client::ControlClient, DiskUsageRequest, DiskUsageResponse, InfoRequest, InfoResponse,
    ListWorkersRequest, ListWorkersResponse,
};

use anyhow::Result;
use clap::Parser;
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

use metrics::scrape_and_record;

/// BuildKit reporting agent — gRPC sidecar for status/metrics (builds, cache, workers).
#[derive(Parser, Debug)]
#[command(name = "buildkit-agent")]
struct Args {
    /// BuildKit gRPC endpoint (unix socket path or unix:///path)
    #[arg(long, env = "BUILDKIT_ADDR", default_value = "unix:///run/buildkit/buildkitd.sock")]
    addr: String,

    /// Metrics HTTP listen address
    #[arg(long, env = "METRICS_ADDR", default_value = "0.0.0.0:9090")]
    metrics_addr: String,

    /// Scrape interval for BuildKit Control API
    #[arg(long, env = "SCRAPE_INTERVAL_SECS", default_value = "15")]
    scrape_interval_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("buildkit_agent=info".parse()?))
        .init();

    let args = Args::parse();

    let socket_path = args
        .addr
        .strip_prefix("unix://")
        .unwrap_or(args.addr.as_str());
    let path = PathBuf::from(socket_path);

    let metrics_handle = metrics::install_recorder();
    let scrape_interval = Duration::from_secs(args.scrape_interval_secs);

    // Background: periodically scrape BuildKit Control API and update metrics
    let path_clone = path.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = scrape_once(&path_clone).await {
                tracing::warn!(err = %e, "scrape failed");
            }
            tokio::time::sleep(scrape_interval).await;
        }
    });

    // HTTP server for Prometheus /metrics
    let listener = tokio::net::TcpListener::bind(&args.metrics_addr).await?;
    tracing::info!(addr = %args.metrics_addr, "metrics listening");
    let handle = metrics_handle.clone();
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let h = handle.clone();
            async move {
                let body = h.render();
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; charset=utf-8",
                    )],
                    body,
                )
            }
        }),
    );
    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

async fn scrape_once(socket_path: &PathBuf) -> Result<()> {
    let path = socket_path.clone();
    let channel = Endpoint::try_from("http://[::]:0")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;

    let mut client = ControlClient::new(channel);

    let info: InfoResponse = client.info(tonic::Request::new(InfoRequest {})).await?.into_inner();
    let workers: ListWorkersResponse = client
        .list_workers(tonic::Request::new(ListWorkersRequest { filter: vec![] }))
        .await?
        .into_inner();
    let disk: DiskUsageResponse = client
        .disk_usage(tonic::Request::new(DiskUsageRequest {
            filter: vec![],
            age_limit: 0,
        }))
        .await?
        .into_inner();

    scrape_and_record(info, workers, disk);
    Ok(())
}
