use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    LogExporter, MetricExporter, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use pyroscope::pyroscope::{PyroscopeAgent, PyroscopeAgentBuilder, PyroscopeAgentRunning};
use pyroscope_pprofrs::{pprof_backend, PprofConfig};
use std::collections::HashMap;
use std::time::Duration;
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Wire protocol for the OTLP transport. Different OTel-compliant backends
/// have different preferences:
/// - Tempo (Grafana Cloud): gRPC on port 443
/// - Loki / Mimir (Grafana Cloud): HTTP only
/// - Alloy / OpenTelemetry Collector: typically gRPC on 4317 or HTTP on 4318
#[allow(dead_code)] // Both variants are part of the public API; main may only use one.
#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    /// OTLP over gRPC (HTTP/2 + protobuf). Default for most collectors and Tempo.
    Grpc,
    /// OTLP over HTTP/1.1 + protobuf. Required for Loki / Mimir in Grafana Cloud.
    Http,
}

/// OTLP transport config. Each pillar (logs/traces/metrics) takes its own
/// `OtlpConfig` so different signals can go to different endpoints/protocols
/// (e.g. logs over HTTP to Loki Cloud, traces over gRPC to Tempo Cloud).
#[derive(Clone)]
pub struct OtlpConfig {
    /// OTLP endpoint, e.g. `http://localhost:4317` (gRPC alloy/collector
    /// sidecar), `https://tempo-prod-08.grafana.net:443` (Tempo Cloud over
    /// gRPC), or `https://logs-prod-08.grafana.net/otlp` (Loki Cloud over
    /// HTTP). Use `https://...` for TLS.
    pub endpoint: String,
    /// Wire protocol. Use [`Protocol::Grpc`] for collectors and Tempo;
    /// [`Protocol::Http`] for Loki/Mimir cloud endpoints.
    pub protocol: Protocol,
    /// Headers attached to every OTLP request. Examples:
    /// - `{"authorization": "Bearer <token>"}` for OAuth
    /// - `{"authorization": "Basic <base64(user:pass)>"}` for basic auth
    /// - `{"x-api-key": "<key>"}` for vendors that use API keys
    ///
    /// Empty map = no auth headers (typical for in-cluster localhost setups).
    pub headers: HashMap<String, String>,
}

/// Format used by the stdout logger (the `tracing_subscriber::fmt` layer).
/// The stdout logger is **independent** of OTLP log export ([`Builder::with_logs`]):
/// you can have stdout-only logs (no OTLP), OTLP-only logs (no stdout, set
/// [`LogFormat::Off`]), or both at the same time. Logs sent through OTLP to
/// Loki/cloud are always structured (protobuf), regardless of this setting.
#[allow(dead_code)] // All variants are part of the public API.
#[derive(Clone, Copy, Debug, Default)]
pub enum LogFormat {
    /// Multi-line, ANSI-colored, human-readable. Best for local development.
    /// One event spans several lines (target, span context, fields, location).
    #[default]
    Pretty,
    /// One line per event, key=value fields, ANSI colors when stdout is a TTY.
    /// Middle ground between Pretty and Json — readable but compact enough
    /// to fit a terminal width.
    Compact,
    /// Single-line JSON per event. Use this when stdout is scraped by a log
    /// shipper (Promtail, Vector, Fluentd, k8s log driver) — the only format
    /// that survives downstream parsing intact.
    Json,
    /// Disable the stdout logger entirely. Use this when all logs go to OTLP
    /// (via [`Builder::with_logs`]) and stdout would just be noise — e.g. in
    /// containers where Loki ingests via OTel and stdout is unwatched.
    Off,
}

/// Config for the Pyroscope CPU profiling agent. Provide it via
/// `Builder::with_profiling(...)` to enable profiling; omit to disable it.
///
/// Pyroscope's SDK exposes dedicated auth methods (not generic headers like
/// the OTel SDK), so its config is separate from `OtlpConfig`.
pub struct ProfilingConfig {
    /// Pyroscope HTTP push endpoint, e.g. `http://localhost:4040`
    /// or `https://profiles-prod.grafana.net:443`.
    pub endpoint: String,
    /// CPU profile sampling rate in Hz. Standard 100 (~1% CPU overhead).
    pub sample_rate_hz: u32,
    /// Bearer/JWT token. Used by Grafana Cloud Pyroscope and similar vendors.
    pub auth_token: Option<String>,
    /// `(username, password)` tuple for on-prem Pyroscope behind basic auth.
    pub basic_auth: Option<(String, String)>,
}

/// Builder for configuring telemetry. All identity fields have defaults.
/// Each pillar (logs / traces / metrics / profiling) is enabled only when its
/// `with_*(...)` method is called with the appropriate config — type system
/// guarantees that an enabled pillar always has its endpoint and headers set.
pub struct Builder {
    service_name: String,
    service_version: String,
    deployment_environment: String,
    host_name: String,
    log_filter: String,
    log_format: LogFormat,
    logs: Option<OtlpConfig>,
    traces: Option<(OtlpConfig, f64)>,
    metrics: Option<(OtlpConfig, Duration)>,
    profiling: Option<ProfilingConfig>,
}

impl Builder {
    /// Start a new builder with sensible defaults. Override any field via
    /// the corresponding setter. At minimum you should set `service_name`
    /// and call at least one `with_*(...)` to get useful telemetry.
    pub fn new() -> Self {
        Self {
            service_name: "unknown-service".to_string(),
            service_version: "0.0.0".to_string(),
            deployment_environment: "local".to_string(),
            host_name: "unknown".to_string(),
            log_filter: "info".to_string(),
            log_format: LogFormat::default(),
            logs: None,
            traces: None,
            metrics: None,
            profiling: None,
        }
    }

    /// Sets the `service.name` resource attribute. Identifies the application
    /// across the entire observability stack — appears as the `job` label in
    /// Mimir, `service.name` attribute on Tempo spans, `service_name` tag in
    /// Pyroscope, and `service_name` label in Loki. Default `"unknown-service"`.
    pub fn service_name(mut self, v: impl Into<String>) -> Self {
        self.service_name = v.into();
        self
    }

    /// Sets the `service.version` resource attribute. Typically pass
    /// `env!("CARGO_PKG_VERSION").to_string()`. Default `"0.0.0"`.
    pub fn service_version(mut self, v: impl Into<String>) -> Self {
        self.service_version = v.into();
        self
    }

    /// Sets the `deployment.environment` resource attribute. Used to filter
    /// dashboards and queries per environment when the same service runs in
    /// multiple places. Typical values: `"local"`, `"compose"`, `"dev"`,
    /// `"staging"`, `"production"`, `"canary"`. Default `"local"`.
    pub fn deployment_environment(mut self, v: impl Into<String>) -> Self {
        self.deployment_environment = v.into();
        self
    }

    /// Sets the `host_name` Pyroscope tag. Typically the container name,
    /// hostname, pod name, or instance ID — used for breakdown views in
    /// Pyroscope when running multiple instances. Default `"unknown"`.
    pub fn host_name(mut self, v: impl Into<String>) -> Self {
        self.host_name = v.into();
        self
    }

    /// Sets the `tracing-subscriber` `EnvFilter` directive. Default `"info"`.
    /// Example: `"info,my_app=debug,pyroscope=warn,Pyroscope=warn"`.
    pub fn log_filter(mut self, v: impl Into<String>) -> Self {
        self.log_filter = v.into();
        self
    }

    /// Sets the stdout log format. Default [`LogFormat::Pretty`] for local
    /// development. Use [`LogFormat::Json`] when stdout is scraped by a log
    /// shipper (Promtail, Vector, k8s log driver), or [`LogFormat::Compact`]
    /// for a one-line readable middle ground.
    pub fn log_format(mut self, format: LogFormat) -> Self {
        self.log_format = format;
        self
    }

    /// Enable OTLP log export with the given transport (endpoint + protocol +
    /// headers). For shared transport across pillars, `OtlpConfig` is `Clone`.
    pub fn with_logs(mut self, config: OtlpConfig) -> Self {
        self.logs = Some(config);
        self
    }

    /// Enable OTLP trace export with head-based sampling. `sample_rate` is
    /// 0.0–1.0 (1.0 = 100%, 0.1 = 10%).
    pub fn with_traces(mut self, config: OtlpConfig, sample_rate: f64) -> Self {
        self.traces = Some((config, sample_rate));
        self
    }

    /// Enable OTLP metric export with periodic batching. `export_interval`
    /// is how often a batch is flushed (typical 15s).
    pub fn with_metrics(mut self, config: OtlpConfig, export_interval: Duration) -> Self {
        self.metrics = Some((config, export_interval));
        self
    }

    /// Enable Pyroscope CPU profiling. Pyroscope uses its own endpoint and
    /// auth, so it takes a full [`ProfilingConfig`] rather than reusing
    /// `OtlpConfig`.
    pub fn with_profiling(mut self, config: ProfilingConfig) -> Self {
        self.profiling = Some(config);
        self
    }

    /// Build all enabled pipelines and install the global tracing subscriber.
    /// Returns a [`Guard`] that flushes everything on drop — keep it alive
    /// in `main` until the process exits.
    pub fn init(self) -> Guard {
        let resource = Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", self.service_name.clone()),
                KeyValue::new("service.version", self.service_version.clone()),
                KeyValue::new(
                    "deployment.environment",
                    self.deployment_environment.clone(),
                ),
            ])
            .build();

        // === LOGS ===
        let (logger_provider, otel_log_layer) = if let Some(cfg) = &self.logs {
            let exporter = match cfg.protocol {
                Protocol::Grpc => LogExporter::builder()
                    .with_tonic()
                    .with_endpoint(&cfg.endpoint)
                    .with_metadata(headers_to_metadata(&cfg.headers))
                    .build()
                    .expect("failed to build OTLP gRPC log exporter"),
                Protocol::Http => LogExporter::builder()
                    .with_http()
                    .with_endpoint(&cfg.endpoint)
                    .with_headers(cfg.headers.clone())
                    .build()
                    .expect("failed to build OTLP HTTP log exporter"),
            };
            let provider = SdkLoggerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource.clone())
                .build();
            let layer = OpenTelemetryTracingBridge::new(&provider);
            (Some(provider), Some(layer))
        } else {
            (None, None)
        };

        // === TRACES ===
        // Head-based sampling — the decision is made when the trace is created
        // (hash of trace_id). It propagates via the traceparent header so the
        // entire trace tree is consistent (everything sampled or nothing).
        let (tracer_provider, otel_trace_layer) = if let Some((cfg, rate)) = &self.traces {
            let exporter = match cfg.protocol {
                Protocol::Grpc => SpanExporter::builder()
                    .with_tonic()
                    .with_endpoint(&cfg.endpoint)
                    .with_metadata(headers_to_metadata(&cfg.headers))
                    .build()
                    .expect("failed to build OTLP gRPC span exporter"),
                Protocol::Http => SpanExporter::builder()
                    .with_http()
                    .with_endpoint(&cfg.endpoint)
                    .with_headers(cfg.headers.clone())
                    .build()
                    .expect("failed to build OTLP HTTP span exporter"),
            };
            let provider = SdkTracerProvider::builder()
                .with_sampler(Sampler::TraceIdRatioBased(*rate))
                .with_batch_exporter(exporter)
                .with_resource(resource.clone())
                .build();
            let tracer = provider.tracer(self.service_name.clone());
            let layer = tracing_opentelemetry::layer().with_tracer(tracer);
            (Some(provider), Some(layer))
        } else {
            (None, None)
        };

        // === METRICS ===
        let meter_provider = if let Some((cfg, interval)) = &self.metrics {
            let exporter = match cfg.protocol {
                Protocol::Grpc => MetricExporter::builder()
                    .with_tonic()
                    .with_endpoint(&cfg.endpoint)
                    .with_metadata(headers_to_metadata(&cfg.headers))
                    .build()
                    .expect("failed to build OTLP gRPC metric exporter"),
                Protocol::Http => MetricExporter::builder()
                    .with_http()
                    .with_endpoint(&cfg.endpoint)
                    .with_headers(cfg.headers.clone())
                    .build()
                    .expect("failed to build OTLP HTTP metric exporter"),
            };
            let reader = PeriodicReader::builder(exporter)
                .with_interval(*interval)
                .build();
            let provider = SdkMeterProvider::builder()
                .with_reader(reader)
                .with_resource(resource.clone())
                .build();
            opentelemetry::global::set_meter_provider(provider.clone());
            Some(provider)
        } else {
            None
        };

        // === PROFILING (Pyroscope) ===
        // PyroscopeAgentBuilder::new(url, app_name) → .backend(...) → .tags(...) → .build()
        // (pyroscope 0.5.x API — pyroscope 2.x has a 6-arg ::new() but pprofrs 0.2.10 requires 0.5.x).
        // The service_name tag MUST match `service.name` in spans (Tempo's tracesToProfiles
        // maps key=service.name to value=service_name).
        let pyroscope_agent = if let Some(cfg) = &self.profiling {
            let mut builder = PyroscopeAgentBuilder::new(&cfg.endpoint, &self.service_name)
                .backend(pprof_backend(
                    PprofConfig::new().sample_rate(cfg.sample_rate_hz),
                ))
                .tags(vec![
                    ("service_name", self.service_name.as_str()),
                    ("service_version", self.service_version.as_str()),
                    (
                        "deployment_environment",
                        self.deployment_environment.as_str(),
                    ),
                    ("host_name", self.host_name.as_str()),
                ]);
            if let Some(token) = &cfg.auth_token {
                builder = builder.auth_token(token);
            }
            if let Some((user, pass)) = &cfg.basic_auth {
                builder = builder.basic_auth(user.clone(), pass.clone());
            }
            let agent = builder.build().expect("failed to build Pyroscope agent");
            Some(agent.start().expect("failed to start Pyroscope agent"))
        } else {
            None
        };

        // === SUBSCRIBER ===
        // tracing_subscriber has a blanket `impl<L, S> Layer<S> for Option<L>`,
        // so passing `None` for a disabled pillar's layer is a no-op.
        // try_init() instead of init() so a second test calling init() in the same
        // process doesn't panic on a duplicate set_global_default. In production
        // init() is called once at startup — if the subscriber is already set,
        // someone is setting up telemetry twice (a bug), but we tolerate it silently.
        //
        // Each format method on fmt::layer() returns a different concrete type;
        // .boxed() on the Layer trait erases them to a uniform Box<dyn Layer>.
        use tracing_subscriber::Layer;
        let fmt_layer = match self.log_format {
            LogFormat::Pretty => Some(tracing_subscriber::fmt::layer().pretty().boxed()),
            LogFormat::Compact => Some(tracing_subscriber::fmt::layer().compact().boxed()),
            LogFormat::Json => Some(tracing_subscriber::fmt::layer().json().boxed()),
            LogFormat::Off => None,
        };
        let _ = tracing_subscriber::registry()
            .with(EnvFilter::new(&self.log_filter))
            .with(fmt_layer)
            .with(otel_trace_layer) // tracing::Span -> OTel Span (must come before the log layer)
            .with(otel_log_layer) // tracing::Event -> OTel LogRecord (with trace_id from the active span)
            .try_init();

        Guard {
            logger_provider,
            tracer_provider,
            meter_provider,
            pyroscope_agent,
        }
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a user-supplied `HashMap<String, String>` of headers into a
/// `tonic::metadata::MetadataMap` suitable for an OTLP gRPC exporter.
/// Invalid header names/values (non-ASCII, control chars, etc.) are silently
/// skipped — this is best-effort, not validation.
fn headers_to_metadata(headers: &HashMap<String, String>) -> MetadataMap {
    let mut metadata = MetadataMap::new();
    for (k, v) in headers {
        if let (Ok(key), Ok(value)) = (
            MetadataKey::from_bytes(k.as_bytes()),
            MetadataValue::try_from(v),
        ) {
            metadata.insert(key, value);
        }
    }
    metadata
}

/// RAII guard — on drop, flushes whichever OTel SDK providers and Pyroscope
/// agent were enabled. Hold it in `main` until the process exits.
pub struct Guard {
    logger_provider: Option<SdkLoggerProvider>,
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    /// `Option<...>` is required because `agent.shutdown()` consumes the agent
    /// (mut self), so we have to `.take()` it inside Drop::drop.
    pyroscope_agent: Option<PyroscopeAgent<PyroscopeAgentRunning>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Pyroscope first — graceful flush of the last samples before the
        // process dies. agent.stop() transitions to Ready state, agent.shutdown()
        // consumes it. .take() because shutdown(mut self) requires ownership.
        if let Some(agent) = self.pyroscope_agent.take() {
            if let Ok(ready) = agent.stop() {
                ready.shutdown();
            }
        }
        if let Some(p) = &self.logger_provider {
            let _ = p.shutdown();
        }
        if let Some(p) = &self.tracer_provider {
            let _ = p.shutdown();
        }
        if let Some(p) = &self.meter_provider {
            let _ = p.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // pprof-rs installs a global signal handler — two instances cannot run
    // concurrently in the same process. Serialize the tests via a static Mutex.
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn test_builder() -> Builder {
        Builder::new()
            .service_name("test-service")
            .service_version("0.0.0")
            .deployment_environment("test")
            .host_name("test-host")
            .log_filter("info")
    }

    fn fake_grpc() -> OtlpConfig {
        OtlpConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            protocol: Protocol::Grpc,
            headers: HashMap::new(),
        }
    }

    fn fake_http() -> OtlpConfig {
        OtlpConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            protocol: Protocol::Http,
            headers: HashMap::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_no_pillars_works() {
        let _lock = TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // No with_*() calls — every pipeline disabled. init() must not panic.
        let _guard = test_builder().init();
        tracing::info!("hello from test");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_only_logs_grpc_works() {
        let _lock = TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = test_builder().with_logs(fake_grpc()).init();
        tracing::info!("hello from test");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_only_logs_http_works() {
        let _lock = TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = test_builder().with_logs(fake_http()).init();
        tracing::info!("hello from test");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_all_pillars_sets_global_meter_provider() {
        let _lock = TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cfg = fake_grpc();
        let _guard = test_builder()
            .with_logs(cfg.clone())
            .with_traces(cfg.clone(), 1.0)
            .with_metrics(cfg.clone(), Duration::from_secs(15))
            .with_profiling(ProfilingConfig {
                endpoint: "http://127.0.0.1:1".to_string(),
                sample_rate_hz: 100,
                auth_token: None,
                basic_auth: None,
            })
            .init();
        // After init() the global meter_provider must yield a usable meter,
        // i.e. one we can build a counter on without panicking.
        let meter = opentelemetry::global::meter("test");
        let _counter = meter.u64_counter("test_counter").build();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_mixed_protocols_works() {
        // Cloud-style: logs HTTP, traces gRPC, metrics HTTP — different per pillar.
        let _lock = TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = test_builder()
            .with_logs(fake_http())
            .with_traces(fake_grpc(), 1.0)
            .with_metrics(fake_http(), Duration::from_secs(15))
            .init();
        tracing::info!("hello from test");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_auth_headers_works() {
        let _lock = TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer test-token".to_string());
        let cfg = OtlpConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            protocol: Protocol::Grpc,
            headers,
        };
        let _guard = test_builder()
            .with_logs(cfg.clone())
            .with_traces(cfg, 1.0)
            .init();
        tracing::info!("hello from test");
    }
}
