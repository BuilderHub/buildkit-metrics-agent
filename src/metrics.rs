//! Prometheus metrics for BuildKit status (info, workers, cache).

use crate::generated::{DiskUsageResponse, InfoResponse, ListWorkersResponse};
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::OnceLock;

static RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn install_recorder() -> PrometheusHandle {
    RECORDER
        .get_or_init(|| metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder().expect("metrics recorder"))
        .clone()
}

/// Update gauges/counters from the latest Control API scrape.
pub fn scrape_and_record(info: InfoResponse, workers: ListWorkersResponse, disk: DiskUsageResponse) {
    // BuildKit version info (we expose as labels or info metric)
    if let Some(v) = info.buildkit_version.as_ref() {
        metrics::gauge!(
            "buildkit_info",
            1.0,
            "version" => v.version.clone(),
            "revision" => v.revision.clone()
        );
    }

    // Worker count
    let n = workers.record.len() as f64;
    metrics::gauge!("buildkit_workers_total", n);

    // Cache / disk usage: total size and record count
    let total_size: i64 = disk.record.iter().map(|r| r.size).sum();
    let count = disk.record.len() as f64;
    metrics::gauge!("buildkit_cache_records_total", count);
    metrics::gauge!("buildkit_cache_size_bytes", total_size as f64);

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
            size as f64,
            "record_type" => record_type
        );
    }
}
