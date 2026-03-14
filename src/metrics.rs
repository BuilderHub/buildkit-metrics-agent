//! Prometheus metrics for BuildKit status (info, workers, cache, builds).

use crate::generated::{BuildHistoryRecord, DiskUsageResponse, InfoResponse, ListWorkersResponse};
use metrics_exporter_prometheus::{Matcher, PrometheusHandle};
use std::sync::OnceLock;
use std::time::SystemTime;

static RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

const BUILD_DURATION_BUCKETS: &[f64] = &[
    1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

pub fn install_recorder() -> PrometheusHandle {
    RECORDER
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Full("buildkit_build_duration_seconds".to_string()),
                    BUILD_DURATION_BUCKETS,
                )
                .expect("valid histogram buckets")
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
        let (succeeded, failed) = if r.error.as_ref().is_some_and(|e| e.code != 0) {
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

        if let (Some(created), Some(completed)) = (&r.created_at, &r.completed_at) {
            let Ok(created) = SystemTime::try_from(*created) else { continue };
            let Ok(completed) = SystemTime::try_from(*completed) else { continue };
            if let Ok(duration) = completed.duration_since(created) {
                metrics::histogram!("buildkit_build_duration_seconds")
                    .record(duration.as_secs_f64());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{BuildError, BuildkitVersion, UsageRecord, WorkerRecord};

    fn recorder() -> (metrics_exporter_prometheus::PrometheusRecorder, PrometheusHandle) {
        let rec = metrics_exporter_prometheus::PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Full("buildkit_build_duration_seconds".to_string()),
                BUILD_DURATION_BUCKETS,
            )
            .unwrap()
            .build_recorder();
        let handle = rec.handle();
        (rec, handle)
    }

    fn render_with(
        rec: &metrics_exporter_prometheus::PrometheusRecorder,
        handle: &PrometheusHandle,
        info: InfoResponse,
        workers: ListWorkersResponse,
        disk: DiskUsageResponse,
        builds: Vec<BuildHistoryRecord>,
    ) -> String {
        metrics::with_local_recorder(rec, || scrape_and_record(info, workers, disk, builds));
        handle.render()
    }

    fn empty_info() -> InfoResponse {
        InfoResponse {
            buildkit_version: None,
        }
    }

    fn empty_workers() -> ListWorkersResponse {
        ListWorkersResponse { record: vec![] }
    }

    fn empty_disk() -> DiskUsageResponse {
        DiskUsageResponse { record: vec![] }
    }

    fn ts(seconds: i64, nanos: i32) -> prost_types::Timestamp {
        prost_types::Timestamp { seconds, nanos }
    }

    fn build_record(
        created: Option<prost_types::Timestamp>,
        completed: Option<prost_types::Timestamp>,
        error: Option<BuildError>,
        cached_steps: i32,
        total_steps: i32,
    ) -> BuildHistoryRecord {
        BuildHistoryRecord {
            r#ref: "test-ref".into(),
            frontend: "dockerfile.v0".into(),
            error,
            created_at: created,
            completed_at: completed,
            num_cached_steps: cached_steps,
            num_total_steps: total_steps,
            num_completed_steps: total_steps,
        }
    }

    // -- Info / workers / cache gauges --

    #[test]
    fn records_buildkit_info() {
        let (rec, handle) = recorder();
        let info = InfoResponse {
            buildkit_version: Some(BuildkitVersion {
                package: String::new(),
                version: "0.14.1".into(),
                revision: "abc123".into(),
            }),
        };
        let out = render_with(&rec, &handle, info, empty_workers(), empty_disk(), vec![]);
        assert!(out.contains(r#"buildkit_info{version="0.14.1",revision="abc123"} 1"#));
    }

    #[test]
    fn records_worker_count() {
        let (rec, handle) = recorder();
        let workers = ListWorkersResponse {
            record: vec![
                WorkerRecord::default(),
                WorkerRecord::default(),
                WorkerRecord::default(),
            ],
        };
        let out = render_with(&rec, &handle, empty_info(), workers, empty_disk(), vec![]);
        assert!(out.contains("buildkit_workers_total 3"));
    }

    #[test]
    fn records_cache_metrics() {
        let (rec, handle) = recorder();
        let disk = DiskUsageResponse {
            record: vec![
                UsageRecord {
                    size: 1000,
                    record_type: "regular".into(),
                    ..Default::default()
                },
                UsageRecord {
                    size: 500,
                    record_type: "regular".into(),
                    ..Default::default()
                },
                UsageRecord {
                    size: 200,
                    record_type: "source.git.checkout".into(),
                    ..Default::default()
                },
            ],
        };
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), disk, vec![]);
        assert!(out.contains("buildkit_cache_records_total 3"));
        assert!(out.contains("buildkit_cache_size_bytes 1700"));
        assert!(out.contains(r#"buildkit_cache_size_by_type_bytes{record_type="regular"} 1500"#));
        assert!(out.contains(
            r#"buildkit_cache_size_by_type_bytes{record_type="source.git.checkout"} 200"#
        ));
    }

    #[test]
    fn empty_record_type_becomes_unknown() {
        let (rec, handle) = recorder();
        let disk = DiskUsageResponse {
            record: vec![UsageRecord {
                size: 42,
                record_type: String::new(),
                ..Default::default()
            }],
        };
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), disk, vec![]);
        assert!(out.contains(r#"buildkit_cache_size_by_type_bytes{record_type="unknown"} 42"#));
    }

    // -- Build counters --

    #[test]
    fn records_successful_build_counters() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(None, None, None, 3, 10)];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);
        assert!(out.contains("buildkit_builds_total 1"));
        assert!(out.contains("buildkit_builds_succeeded_total 1"));
        assert!(out.contains("buildkit_builds_failed_total 0"));
        assert!(out.contains("buildkit_builds_cached_steps_total 3"));
        assert!(out.contains("buildkit_builds_total_steps_total 10"));
    }

    #[test]
    fn records_failed_build_counters() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(
            None,
            None,
            Some(BuildError {
                code: 1,
                message: "build failed".into(),
            }),
            0,
            5,
        )];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);
        assert!(out.contains("buildkit_builds_total 1"));
        assert!(out.contains("buildkit_builds_succeeded_total 0"));
        assert!(out.contains("buildkit_builds_failed_total 1"));
    }

    #[test]
    fn error_code_zero_counts_as_success() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(
            None,
            None,
            Some(BuildError {
                code: 0,
                message: String::new(),
            }),
            0,
            1,
        )];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);
        assert!(out.contains("buildkit_builds_succeeded_total 1"));
        assert!(out.contains("buildkit_builds_failed_total 0"));
    }

    // -- Build duration histogram --

    #[test]
    fn records_build_duration() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(
            Some(ts(1_700_000_000, 0)),
            Some(ts(1_700_000_045, 500_000_000)),
            None,
            0,
            1,
        )];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);

        // 45.5s falls in the 60s bucket
        assert!(out.contains("buildkit_build_duration_seconds_bucket{le=\"30\"} 0"));
        assert!(out.contains("buildkit_build_duration_seconds_bucket{le=\"60\"} 1"));
        assert!(out.contains("buildkit_build_duration_seconds_sum 45.5"));
        assert!(out.contains("buildkit_build_duration_seconds_count 1"));
    }

    #[test]
    fn no_histogram_without_timestamps() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(None, None, None, 0, 1)];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);
        assert!(!out.contains("buildkit_build_duration_seconds"));
    }

    #[test]
    fn no_histogram_with_only_created_at() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(Some(ts(1_700_000_000, 0)), None, None, 0, 1)];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);
        assert!(!out.contains("buildkit_build_duration_seconds"));
    }

    #[test]
    fn no_histogram_when_completed_before_created() {
        let (rec, handle) = recorder();
        let builds = vec![build_record(
            Some(ts(1_700_000_100, 0)),
            Some(ts(1_700_000_000, 0)),
            None,
            0,
            1,
        )];
        let out = render_with(&rec, &handle, empty_info(), empty_workers(), empty_disk(), builds);
        assert!(!out.contains("buildkit_build_duration_seconds"));
    }

    #[test]
    fn zero_gauges_on_empty_input() {
        let (rec, handle) = recorder();
        let out = render_with(
            &rec,
            &handle,
            empty_info(),
            empty_workers(),
            empty_disk(),
            vec![],
        );
        assert!(out.contains("buildkit_workers_total 0"));
        assert!(out.contains("buildkit_cache_records_total 0"));
        assert!(out.contains("buildkit_cache_size_bytes 0"));
        assert!(!out.contains("buildkit_info"));
    }
}
