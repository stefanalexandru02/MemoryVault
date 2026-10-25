//! Prometheus exporter (spec 15). Metric names carry the `memsvc_` prefix.
//!
//! [`prometheus_handle`] installs the global `metrics` recorder on first use; record with the
//! `metrics::counter!` / `histogram!` / `gauge!` macros anywhere. The server renders the handle at
//! `GET /metrics` and runs its upkeep.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// The process-wide recorder handle (installed on first call). If another recorder was installed
/// first, a detached handle is returned and `/metrics` renders nothing.
pub fn prometheus_handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let builder = PrometheusBuilder::new();
        let recorder = builder.build_recorder();
        let handle = recorder.handle();
        if metrics::set_global_recorder(recorder).is_err() {
            tracing::warn!("a metrics recorder was already installed; /metrics will be empty");
        }
        handle
    })
}
