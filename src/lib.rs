//! BuildKit metrics agent library: gRPC scrape loop and Prometheus `/metrics` serving.
//!
//! On process shutdown (Ctrl+C or HTTP server exit), a [`tokio::sync::watch`] flag stops the
//! [`supervised_scrape_loop`], which aborts the inner [`scrape_loop`] task; the supervisor does not
//! respawn after that.

pub mod generated;
pub mod metrics;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use generated::{
    control_client::ControlClient, BuildHistoryEventType, BuildHistoryRequest, DiskUsageRequest,
    DiskUsageResponse, InfoRequest, InfoResponse, ListWorkersRequest, ListWorkersResponse,
};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::PrometheusHandle;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use axum::Router;

/// BuildKit Metrics Agent — a lightweight application that scrapes and exposes BuildKit metrics.
#[derive(Parser, Debug)]
#[command(name = "buildkit-metrics-agent")]
pub struct Args {
    /// BuildKit gRPC endpoint (unix socket path or unix:///path)
    #[arg(
        long,
        env = "BUILDKIT_ADDR",
        default_value = "unix:///run/buildkit/buildkitd.sock"
    )]
    pub addr: String,

    /// Metrics HTTP listen address
    #[arg(long, env = "METRICS_ADDR", default_value = "0.0.0.0:9090")]
    pub metrics_addr: String,

    /// Scrape interval for BuildKit Control API
    #[arg(long, env = "SCRAPE_INTERVAL_SECS", default_value = "15")]
    pub scrape_interval_secs: u64,
}

/// Normalize `BUILDKIT_ADDR` to a filesystem path (strip optional `unix://` prefix).
pub fn socket_path_from_addr(addr: &str) -> PathBuf {
    PathBuf::from(
        addr.strip_prefix("unix://")
            .unwrap_or(addr),
    )
}

pub async fn run() -> Result<()> {
    run_with(Args::parse()).await
}

/// Time to wait after the inner [`scrape_loop`] task panics, returns, or is cancelled (without
/// shutdown) before respawning.
const SUPERVISOR_BACKOFF: Duration = Duration::from_secs(1);

/// `true` means stop [`supervised_scrape_loop`]; `false` means wait finished and a new child may
/// be spawned.
async fn supervisor_backoff_or_stop(shutdown_rx: &mut watch::Receiver<bool>, d: Duration) -> bool {
    let sleep = tokio::time::sleep(d);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;
            c = shutdown_rx.changed() => {
                if c.is_err() {
                    // All senders dropped; process is stopping.
                    return true;
                }
                if *shutdown_rx.borrow() {
                    return true;
                }
            }
            _ = &mut sleep => {
                return false;
            }
        }
    }
}

/// Respawns the inner [`scrape_loop`] when it panics or exits, until `shutdown_rx` is `true` or
/// the sender is dropped.
async fn supervised_scrape_loop(
    path: PathBuf,
    seen_refs: Arc<Mutex<HashSet<String>>>,
    scrape_interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        let mut child = tokio::spawn(scrape_loop(
            path.clone(),
            Arc::clone(&seen_refs),
            scrape_interval,
        ));
        let child_abort = child.abort_handle();

        let res = 'join: loop {
            tokio::select! {
                // Shutdown first when both the inner task and a watch notification are ready.
                biased;
                c = shutdown_rx.changed() => {
                    if c.is_err() {
                        // All senders dropped; process is stopping.
                        child_abort.abort();
                        return;
                    }
                    if *shutdown_rx.borrow() {
                        child_abort.abort();
                        // Wait for the inner task to complete so the JoinHandle is not leaked.
                        let r = child.await;
                        break 'join r;
                    }
                }
                r = &mut child => {
                    break 'join r;
                }
            }
        };

        if *shutdown_rx.borrow() {
            if let Err(e) = &res {
                if e.is_cancelled() {
                    return;
                }
            }
        }

        match res {
            Ok(()) => {
                tracing::error!("scrape_loop task exited without error; restarting after backoff");
                if supervisor_backoff_or_stop(&mut shutdown_rx, SUPERVISOR_BACKOFF).await {
                    return;
                }
            }
            Err(e) if e.is_panic() => {
                match e.try_into_panic() {
                    Ok(payload) => {
                        tracing::error!(?payload, "scrape_loop task panicked; restarting after backoff");
                    }
                    Err(_) => {
                        tracing::error!("scrape_loop task panicked; restarting after backoff");
                    }
                }
                if supervisor_backoff_or_stop(&mut shutdown_rx, SUPERVISOR_BACKOFF).await {
                    return;
                }
            }
            Err(e) if e.is_cancelled() => {
                if *shutdown_rx.borrow() {
                    return;
                }
                tracing::warn!("scrape_loop task cancelled unexpectedly; restarting after backoff");
                if supervisor_backoff_or_stop(&mut shutdown_rx, SUPERVISOR_BACKOFF).await {
                    return;
                }
            }
            Err(e) => {
                tracing::error!(%e, "scrape_loop task failed; restarting after backoff");
                if supervisor_backoff_or_stop(&mut shutdown_rx, SUPERVISOR_BACKOFF).await {
                    return;
                }
            }
        }
    }
}

pub(crate) async fn run_with(args: Args) -> Result<()> {
    run_with_graceful(
        args,
        async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        },
    )
    .await
}

/// Same as [`run_with`], but the HTTP server’s graceful-shutdown signal is supplied for tests or
/// embedding (e.g. `std::future::ready(())` to stop immediately).
pub(crate) async fn run_with_graceful(
    args: Args,
    http_shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let path = socket_path_from_addr(&args.addr);
    let metrics_handle = metrics::install_recorder();
    let scrape_interval = Duration::from_secs(args.scrape_interval_secs);

    let seen_refs: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let listener = tokio::net::TcpListener::bind(&args.metrics_addr)
        .await
        .with_context(|| format!("bind metrics listener on {}", args.metrics_addr))?;

    let supervisor = tokio::spawn(supervised_scrape_loop(
        path,
        Arc::clone(&seen_refs),
        scrape_interval,
        shutdown_rx,
    ));

    let serve_result = serve_metrics_from_listener(listener, metrics_handle, http_shutdown).await;

    // Stops the supervisor from respawning; the supervisor also aborts the inner scrape in select!.
    // Await the supervisor so the inner `scrape_loop` is joined (Tokio does not drop child tasks when
    // a parent is cancelled).
    let _ = shutdown_tx.send(true);
    let _ = supervisor.await;
    serve_result
}

#[cfg(test)]
async fn serve_metrics(addr: &str, handle: PrometheusHandle) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind metrics listener on {addr}"))?;
    // Never signal shutdown from axum: tests and callers rely on task abort to stop the server.
    serve_metrics_from_listener(listener, handle, std::future::pending::<()>()).await
}

pub(crate) async fn serve_metrics_from_listener(
    listener: tokio::net::TcpListener,
    handle: PrometheusHandle,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    tracing::info!(
        addr = %listener.local_addr().context("metrics listener local_addr")?,
        "metrics listening"
    );
    let app = metrics_router(handle);
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

pub(crate) fn metrics_router(handle: PrometheusHandle) -> Router {
    Router::new().route(
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
    )
}

type ScrapeBoxFut = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

async fn scrape_loop_impl<F>(
    socket_path: PathBuf,
    seen_refs: Arc<Mutex<HashSet<String>>>,
    scrape_interval: Duration,
    mut scrape: F,
) where
    F: FnMut(&Path, &Arc<Mutex<HashSet<String>>>) -> ScrapeBoxFut,
{
    tokio::time::sleep(Duration::from_secs(1)).await;
    loop {
        if let Err(e) = scrape(socket_path.as_path(), &seen_refs).await {
            tracing::warn!(err = %e, "scrape failed");
        }
        tokio::time::sleep(scrape_interval).await;
    }
}

pub async fn scrape_loop(
    socket_path: PathBuf,
    seen_refs: Arc<Mutex<HashSet<String>>>,
    scrape_interval: Duration,
) {
    scrape_loop_impl(
        socket_path,
        seen_refs,
        scrape_interval,
        |p, s| {
            let path = p.to_path_buf();
            let seen = Arc::clone(s);
            Box::pin(async move { scrape_once(path.as_path(), &seen).await })
        },
    )
    .await
}

pub async fn scrape_once(socket_path: &Path, seen_refs: &Arc<Mutex<HashSet<String>>>) -> Result<()> {
    let channel = connect_control_channel(socket_path)
        .await
        .context("connect buildkit gRPC channel")?;
    let mut client = ControlClient::new(channel);
    scrape_once_with(&mut client, seen_refs).await
}

async fn connect_control_channel(socket_path: &Path) -> Result<Channel> {
    let path = socket_path.to_path_buf();
    Endpoint::try_from("http://[::]:0")
        .context("parse tonic endpoint placeholder")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .context("unix connector handshake")
}

#[async_trait]
pub(crate) trait ControlApi: Send {
    async fn fetch_info(&mut self) -> Result<InfoResponse>;
    async fn fetch_workers(&mut self) -> Result<ListWorkersResponse>;
    async fn fetch_disk(&mut self) -> Result<DiskUsageResponse>;
    async fn fetch_completed_builds(&mut self) -> Result<Vec<generated::BuildHistoryRecord>>;
}

#[async_trait]
impl ControlApi for ControlClient<Channel> {
    async fn fetch_info(&mut self) -> Result<InfoResponse> {
        Ok(self
            .info(tonic::Request::new(InfoRequest {}))
            .await
            .context("buildkit Control/Info")?
            .into_inner())
    }

    async fn fetch_workers(&mut self) -> Result<ListWorkersResponse> {
        Ok(self
            .list_workers(tonic::Request::new(ListWorkersRequest { filter: vec![] }))
            .await
            .context("buildkit Control/ListWorkers")?
            .into_inner())
    }

    async fn fetch_disk(&mut self) -> Result<DiskUsageResponse> {
        Ok(self
            .disk_usage(tonic::Request::new(DiskUsageRequest {
                filter: vec![],
                age_limit: 0,
            }))
            .await
            .context("buildkit Control/DiskUsage")?
            .into_inner())
    }

    async fn fetch_completed_builds(&mut self) -> Result<Vec<generated::BuildHistoryRecord>> {
        let mut build_stream = self
            .listen_build_history(tonic::Request::new(BuildHistoryRequest {
                early_exit: true,
                ..Default::default()
            }))
            .await
            .context("buildkit Control/ListenBuildHistory")?
            .into_inner();

        let mut completed = Vec::new();
        while let Some(event) = build_stream
            .message()
            .await
            .context("build history stream message")?
        {
            if event.r#type() == BuildHistoryEventType::Complete {
                if let Some(record) = event.record {
                    completed.push(record);
                }
            }
        }
        Ok(completed)
    }
}

pub(crate) async fn scrape_once_with(
    client: &mut impl ControlApi,
    seen_refs: &Arc<Mutex<HashSet<String>>>,
) -> Result<()> {
    let info = client.fetch_info().await?;
    let workers = client.fetch_workers().await?;
    let disk = client.fetch_disk().await?;
    let completed = client.fetch_completed_builds().await?;

    let new_records = take_new_build_records(seen_refs, completed);
    metrics::scrape_and_record(info, workers, disk, new_records);
    Ok(())
}

fn take_new_build_records(
    seen_refs: &Arc<Mutex<HashSet<String>>>,
    completed: Vec<generated::BuildHistoryRecord>,
) -> Vec<generated::BuildHistoryRecord> {
    let mut seen = seen_refs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    completed
        .into_iter()
        .filter(|r| seen.insert(r.r#ref.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{
        types::{BuildkitVersion, WorkerRecord},
        BuildHistoryRecord,
    };
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    #[test]
    fn socket_path_strips_unix_scheme() {
        assert_eq!(
            socket_path_from_addr("unix:///run/buildkit/buildkitd.sock"),
            PathBuf::from("/run/buildkit/buildkitd.sock")
        );
    }

    #[test]
    fn socket_path_passes_through_plain_path() {
        assert_eq!(
            socket_path_from_addr("/tmp/buildkit.sock"),
            PathBuf::from("/tmp/buildkit.sock")
        );
    }

    #[derive(Clone, Default)]
    struct FakeControl {
        info: InfoResponse,
        workers: ListWorkersResponse,
        disk: DiskUsageResponse,
        builds: Vec<BuildHistoryRecord>,
        fail_builds_with: Option<String>,
    }

    #[async_trait]
    impl ControlApi for FakeControl {
        async fn fetch_info(&mut self) -> Result<InfoResponse> {
            Ok(self.info.clone())
        }

        async fn fetch_workers(&mut self) -> Result<ListWorkersResponse> {
            Ok(self.workers.clone())
        }

        async fn fetch_disk(&mut self) -> Result<DiskUsageResponse> {
            Ok(self.disk.clone())
        }

        async fn fetch_completed_builds(&mut self) -> Result<Vec<BuildHistoryRecord>> {
            if let Some(msg) = &self.fail_builds_with {
                return Err(anyhow::anyhow!("{msg}"));
            }
            Ok(self.builds.clone())
        }
    }

    #[test]
    fn scrape_once_with_happy_path_records_metrics() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let rec = PrometheusBuilder::new().build_recorder();
        let handle = rec.handle();

        let mut fake = FakeControl {
            info: InfoResponse {
                buildkit_version: Some(BuildkitVersion {
                    package: String::new(),
                    version: "1.0.0".into(),
                    revision: "rev".into(),
                    ..Default::default()
                }),
            },
            workers: ListWorkersResponse {
                record: vec![WorkerRecord::default()],
            },
            disk: DiskUsageResponse { record: vec![] },
            builds: vec![BuildHistoryRecord {
                r#ref: "ref-a".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        ::metrics::with_local_recorder(&rec, || {
            rt.block_on(scrape_once_with(&mut fake, &seen))
                .expect("scrape");
        });

        let body = handle.render();
        assert!(body.contains(r#"buildkit_info{version="1.0.0",revision="rev"} 1"#));
        assert!(body.contains("buildkit_workers_total 1"));
    }

    #[tokio::test]
    async fn scrape_once_with_propagates_build_stream_error() {
        let mut fake = FakeControl {
            fail_builds_with: Some("stream failed".into()),
            ..Default::default()
        };
        let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let err = scrape_once_with(&mut fake, &seen).await.unwrap_err();
        assert!(err.to_string().contains("stream failed"));
    }

    #[test]
    fn take_new_build_records_recovers_from_poisoned_mutex() {
        let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let bad = catch_unwind(AssertUnwindSafe(|| {
            let _g = seen.lock().unwrap();
            panic!("poison");
        }));
        assert!(bad.is_err());

        let completed = vec![BuildHistoryRecord {
            r#ref: "new-ref".into(),
            ..Default::default()
        }];
        let out = take_new_build_records(&seen, completed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].r#ref, "new-ref");
    }

    #[test]
    fn take_new_build_records_skips_duplicate_refs() {
        let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        {
            seen.lock().unwrap().insert("old".into());
        }
        let completed = vec![
            BuildHistoryRecord {
                r#ref: "old".into(),
                ..Default::default()
            },
            BuildHistoryRecord {
                r#ref: "fresh".into(),
                ..Default::default()
            },
        ];
        let out = take_new_build_records(&seen, completed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].r#ref, "fresh");
    }

    #[test]
    fn metrics_router_serves_prometheus_text() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let rec = PrometheusBuilder::new().build_recorder();
        let handle = rec.handle();

        ::metrics::with_local_recorder(&rec, || {
            rt.block_on(async {
                let app = metrics_router(handle.clone());
                let response = app
                    .oneshot(
                        Request::builder()
                            .uri("/metrics")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), axum::http::StatusCode::OK);
                let ctype = response.headers().get(axum::http::header::CONTENT_TYPE);
                assert_eq!(ctype.unwrap(), "text/plain; charset=utf-8");
            });
        });
    }

    static INSTALL_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn install_recorder_global_smoke() {
        let _guard = INSTALL_TEST_LOCK.lock().unwrap();
        let h = metrics::install_recorder();
        let _ = h.render();
    }

    #[tokio::test]
    async fn scrape_once_fails_on_missing_unix_socket() {
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let err = scrape_once(Path::new("/no/such/buildkit.sock"), &seen)
            .await
            .unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("connect buildkit gRPC channel")
                || chain.contains("unix connector handshake")
                || chain.contains("Connection refused")
                || chain.contains("No such file")
                || chain.contains("os error 2"),
            "{chain}"
        );
    }

    #[tokio::test]
    async fn scrape_loop_impl_runs_scrape_after_warmup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let path = PathBuf::from("/tmp/buildkit-metrics-agent-test.sock");
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let task = tokio::spawn(async move {
            scrape_loop_impl(
                path,
                seen,
                Duration::from_secs(3600),
                move |_p, _s| {
                    calls2.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                },
            )
            .await
        });
        // Warmup sleep inside scrape_loop_impl is 1s real time.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
    }

    #[tokio::test]
    async fn serve_metrics_binds_ephemeral_port() {
        let rec = PrometheusBuilder::new().build_recorder();
        let handle = rec.handle();
        let task = tokio::spawn(async move {
            let _ = serve_metrics("127.0.0.1:0", handle).await;
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        task.abort();
    }

    #[tokio::test]
    async fn run_with_spawns_background_scrape() {
        let args = Args {
            addr: "unix:///tmp/buildkit-metrics-agent-no-such.sock".into(),
            metrics_addr: "127.0.0.1:0".into(),
            scrape_interval_secs: 600,
        };
        let task = tokio::spawn(run_with(args));
        tokio::time::sleep(Duration::from_millis(120)).await;
        task.abort();
    }

    #[tokio::test]
    async fn serve_metrics_from_listener_serves_http_get() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let rec = PrometheusBuilder::new().build_recorder();
        let handle = rec.handle();
        let server = tokio::spawn(serve_metrics_from_listener(
            listener,
            handle,
            std::future::pending::<()>(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect");
        stream
            .write_all(
                b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write");
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.expect("read");
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(head.contains("200 OK"), "response: {head:?}");
        server.abort();
    }

    #[tokio::test]
    async fn supervisor_backoff_or_stop_completes_sleep_without_shutdown() {
        let (_tx, mut rx) = watch::channel(false);
        let out = super::supervisor_backoff_or_stop(
            &mut rx,
            Duration::from_millis(40),
        )
        .await;
        assert!(!out, "expected sleep to complete without stop");
    }

    #[tokio::test]
    async fn supervisor_backoff_or_stop_stops_on_watch_true_before_sleep_ends() {
        let (tx, mut rx) = watch::channel(false);
        let t = tx.clone();
        let wake = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            t.send(true).expect("send");
        });
        let out = super::supervisor_backoff_or_stop(
            &mut rx,
            Duration::from_secs(2),
        )
        .await;
        let _ = wake.await;
        assert!(out, "expected shutdown to win over long sleep");
    }

    #[tokio::test]
    async fn supervisor_backoff_or_stop_stops_when_watch_sender_dropped() {
        let (tx, mut rx) = watch::channel(false);
        drop(tx);
        let out = super::supervisor_backoff_or_stop(
            &mut rx,
            Duration::from_secs(10),
        )
        .await;
        assert!(out, "expected RecvError path when all senders dropped");
    }

    #[tokio::test]
    async fn supervised_scrape_loop_stops_on_watch() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let path = PathBuf::from("/tmp/buildkit-metrics-agent-coverage-supervised.sock");
        let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let sup = tokio::spawn(super::supervised_scrape_loop(
            path,
            seen,
            Duration::from_secs(600),
            shutdown_rx,
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown_tx.send(true).expect("shutdown");
        sup.await
            .expect("supervisor should finish cleanly, not panic");
    }

    #[tokio::test]
    async fn run_with_graceful_exits_when_http_shutdown_immediate() {
        let args = Args {
            addr: "unix:///tmp/buildkit-metrics-agent-no-such.sock".into(),
            metrics_addr: "127.0.0.1:0".into(),
            scrape_interval_secs: 600,
        };
        let r = run_with_graceful(args, std::future::ready(())).await;
        assert!(r.is_ok());
    }

    #[test]
    fn serve_metrics_from_listener_completes_on_immediate_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let rec = PrometheusBuilder::new().build_recorder();
        let handle = rec.handle();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind");
        let out: Result<(), anyhow::Error> = ::metrics::with_local_recorder(&rec, || {
            rt.block_on(async { serve_metrics_from_listener(listener, handle, std::future::ready(())).await })
        });
        out.expect("serve");
    }
}
