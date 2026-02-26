//! BuildKit reporting agent: sidecar that talks to BuildKit over gRPC (Control API only)
//! and exposes metrics for builds, cache, and workers.

mod generated;
mod metrics;

use generated::{
    control_client::ControlClient, BuildHistoryEventType, BuildHistoryRequest, DiskUsageRequest,
    DiskUsageResponse, InfoRequest, InfoResponse, ListWorkersRequest, ListWorkersResponse,
};

use anyhow::Result;
use clap::Parser;
use hyper_util::rt::TokioIo;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

use metrics::scrape_and_record;

/// BuildKit reporting agent — gRPC sidecar for status/metrics (builds, cache, workers).
#[derive(Parser, Debug)]
#[command(name = "buildkit-metrics-agent")]
struct Args {
    /// BuildKit gRPC endpoint (unix socket path or unix:///path)
    #[arg(
        long,
        env = "BUILDKIT_ADDR",
        default_value = "unix:///run/buildkit/buildkitd.sock"
    )]
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("buildkit_agent=info".parse()?),
        )
        .init();

    let args = Args::parse();

    let socket_path = args
        .addr
        .strip_prefix("unix://")
        .unwrap_or(args.addr.as_str());
    let path = PathBuf::from(socket_path);

    let metrics_handle = metrics::install_recorder();
    let scrape_interval = Duration::from_secs(args.scrape_interval_secs);

    // Tracks build refs we've already counted so counters only move forward
    // even as BuildKit's history window evicts old records.
    let seen_refs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Background: periodically scrape BuildKit Control API and update metrics.
    // Initial sleep gives buildkitd time to create its socket before the first attempt.
    let path_clone = path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        loop {
            if let Err(e) = scrape_once(&path_clone, Arc::clone(&seen_refs)).await {
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

async fn scrape_once(socket_path: &PathBuf, seen_refs: Arc<Mutex<HashSet<String>>>) -> Result<()> {
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

    let info: InfoResponse = client
        .info(tonic::Request::new(InfoRequest {}))
        .await?
        .into_inner();
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
    // Stream existing build history then close (EarlyExit stops the stream
    // once all current records have been sent).
    let mut build_stream = client
        .listen_build_history(tonic::Request::new(BuildHistoryRequest {
            early_exit: true,
            ..Default::default()
        }))
        .await?
        .into_inner();

    // Collect completed records from the stream before touching the lock.
    let mut completed = Vec::new();
    while let Some(event) = build_stream.message().await? {
        if event.r#type() == BuildHistoryEventType::Complete {
            if let Some(record) = event.record {
                completed.push(record);
            }
        }
    }

    // Filter to only refs we haven't counted yet — lock is not held across any await.
    let new_records = {
        let mut seen = seen_refs.lock().unwrap();
        completed
            .into_iter()
            .filter(|r| seen.insert(r.r#ref.clone()))
            .collect::<Vec<_>>()
    };

    scrape_and_record(info, workers, disk, new_records);
    Ok(())
}
