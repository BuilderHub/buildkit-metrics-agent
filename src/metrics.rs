//! Prometheus metrics for BuildKit status (info, workers, cache).

use crate::generated::{BuildHistoryRecord, DiskUsageResponse, InfoResponse, ListWorkersResponse};
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::OnceLock;

static RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn install_recorder() -> PrometheusHandle {
    RECORDER
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .expect("metrics recorder")
        })
        .clone()
}

/// Update gauges/counters from the latest Control API scrape.
pub fn scrape_and_record(
    info: InfoResponse,
    workers: ListWorkersResponse,
    disk: DiskUsageResponse,
    builds: Vec<BuildHistoryRecord>,
) {
    // BuildKit version info (we expose as labels or info metric)
    if let Some(v) = info.buildkit_version.as_ref() {
        metrics::gauge!(
            "buildkit_info",
            "version" => v.version.clone(),
            "revision" => v.revision.clone()
        )
        .set(1.0);
    }

    // Worker count
    let n = workers.record.len() as f64;
    metrics::gauge!("buildkit_workers_total").set(n);

    // Cache / disk usage: total size and record count
    let total_size: i64 = disk.record.iter().map(|r| r.size).sum();
    let count = disk.record.len() as f64;
    metrics::gauge!("buildkit_cache_records_total").set(count);
    metrics::gauge!("buildkit_cache_size_bytes").set(total_size as f64);

    // Size by record type (e.g. snapshot, content)
    let mut by_type: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for r in &disk.record {
        let t = if r.record_type.is_empty() {
            "unknown".to_string()
        } else {
            r.record_type.clone()
        };
        *by_type.entry(t).or_insert(0) += r.size;
    }
    for (record_type, size) in by_type {
        metrics::gauge!(
            "buildkit_cache_size_by_type_bytes",
            "record_type" => record_type
        )
        .set(size as f64);
    }

    // Increment counters only for builds not yet seen — callers pass only new records.
    for r in &builds {
        let (succeeded, failed) = if r.error.as_ref().map_or(false, |e| e.code != 0) {
            (0u64, 1u64)
        } else {
            (1u64, 0u64)
        };
        metrics::counter!("buildkit_builds_total").increment(1);
        metrics::counter!("buildkit_builds_succeeded_total").increment(succeeded);
        metrics::counter!("buildkit_builds_failed_total").increment(failed);
        metrics::counter!("buildkit_builds_cached_steps_total")
            .increment(r.num_cached_steps as u64);
        metrics::counter!("buildkit_builds_total_steps_total").increment(r.num_total_steps as u64);
    }
}
