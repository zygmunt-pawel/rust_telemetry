# rust_telemetry

Drop-in OpenTelemetry + Pyroscope setup for Rust applications. One Builder API
covers all four pillars of observability:

| Pillar | Backend examples | Status |
|---|---|---|
| **Logs** | Loki, Grafana Cloud Logs, any OTel collector | ✅ |
| **Traces** | Tempo, Grafana Cloud Traces, any OTel collector | ✅ |
| **Metrics** | Mimir, Prometheus, Grafana Cloud Metrics | ✅ |
| **Profiles** (CPU) | Pyroscope, Grafana Cloud Profiles | ✅ |

Each pillar is independently enabled — use one, two, three, or all four. Each
can have its own endpoint, protocol (gRPC or HTTP), and auth headers, so you
can mix-and-match transports (e.g. Tempo gRPC + Loki HTTP for direct push to
Grafana Cloud).

## Why

OpenTelemetry's Rust SDK is modular by design: every signal lives in its own
crate, every backend has its own setup. Pyroscope is a separate ecosystem on
top. Wiring them together for a typical web service is ~150 lines of plumbing
that every project rewrites slightly differently.

This crate is that plumbing, packaged. You get a Builder, you get a Guard, your
`main` stays clean.

## Quick start

`Cargo.toml`:

```toml
[dependencies]
rust_telemetry = { git = "https://github.com/zygmunt-pawel/rust_telemetry" }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
```

`main.rs`:

```rust
use rust_telemetry::{Builder, OtlpConfig, Protocol, ProfilingConfig};
use std::collections::HashMap;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let alloy = OtlpConfig {
        endpoint: "http://localhost:4317".to_string(),
        protocol: Protocol::Grpc,
        headers: HashMap::new(),
    };

    let _guard = Builder::new()
        .service_name("my-app")
        .service_version(env!("CARGO_PKG_VERSION"))
        .deployment_environment("production")
        .host_name("instance-01")
        .log_filter("info,my_app=debug,pyroscope=warn,Pyroscope=warn")
        .with_logs(alloy.clone())
        .with_traces(alloy.clone(), 0.1)
        .with_metrics(alloy.clone(), Duration::from_secs(15))
        .with_profiling(ProfilingConfig {
            endpoint: "http://localhost:4040".to_string(),
            sample_rate_hz: 100,
            auth_token: None,
            basic_auth: None,
        })
        .init();

    tracing::info!("hello from my-app");
    // ...your application...
    // _guard's Drop flushes all pipelines on shutdown.
}
```

That's the whole setup.

## Builder API

```rust
Builder::new()
    // Identity (all have defaults; service_name and service_version
    // should normally be set so signals are identifiable):
    .service_name("my-app")            // default "unknown-service"
    .service_version("1.0.0")          // default "0.0.0"
    .deployment_environment("prod")    // default "local"
    .host_name("instance-01")          // default "unknown"
    .log_filter("info,my_app=debug")   // default "info"

    // Pillars (each is optional; type system enforces full config when enabled):
    .with_logs(otlp_config)
    .with_traces(otlp_config, sample_rate)            // 0.0 .. 1.0
    .with_metrics(otlp_config, export_interval)       // Duration
    .with_profiling(profiling_config)

    // Build everything and install the global tracing subscriber:
    .init()                                            // returns Guard
```

The returned `Guard` is an RAII handle: hold it in `main` until the process
exits. On drop it flushes Pyroscope, then logs/traces/metrics — in that order.

## Configuration shapes

```rust
pub struct OtlpConfig {
    pub endpoint: String,                          // e.g. "http://alloy:4317"
    pub protocol: Protocol,                        // Grpc | Http
    pub headers: HashMap<String, String>,          // empty = no auth
}

pub enum Protocol {
    Grpc,    // OTLP/gRPC — default for collectors and Tempo
    Http,    // OTLP/HTTP — required for Loki/Mimir cloud
}

pub struct ProfilingConfig {
    pub endpoint: String,                          // e.g. "http://localhost:4040"
    pub sample_rate_hz: u32,                       // 100 = standard ~1% CPU overhead
    pub auth_token: Option<String>,                // Bearer/JWT (Grafana Cloud)
    pub basic_auth: Option<(String, String)>,      // (user, pass) for on-prem
}
```

`OtlpConfig` is `Clone`, so you can build it once and pass to multiple pillars
when they share a transport.

## Common scenarios

### 1. Sidecar Alloy / OpenTelemetry Collector (most common)

App pushes everything to a local agent (`localhost:4317`), agent pushes
upstream. Application doesn't know any secrets.

```rust
let alloy = OtlpConfig {
    endpoint: "http://localhost:4317".to_string(),
    protocol: Protocol::Grpc,
    headers: HashMap::new(),
};

Builder::new()
    .service_name("my-app")
    .with_logs(alloy.clone())
    .with_traces(alloy.clone(), 0.1)
    .with_metrics(alloy.clone(), Duration::from_secs(15))
    .with_profiling(ProfilingConfig {
        endpoint: "http://localhost:4040".to_string(),
        sample_rate_hz: 100,
        auth_token: None,
        basic_auth: None,
    })
    .init();
```

### 2. Direct push to Grafana Cloud

Each Grafana Cloud signal has its own endpoint and protocol. Auth is
HTTP Basic with `instance_id:token` base64-encoded.

```rust
use base64::{Engine, engine::general_purpose};

let basic = format!(
    "Basic {}",
    general_purpose::STANDARD.encode(format!("{}:{}", instance_id, api_token)),
);
let auth: HashMap<String, String> = [("authorization".to_string(), basic)]
    .into_iter()
    .collect();

Builder::new()
    .service_name("my-app")
    .deployment_environment("production")
    .with_logs(OtlpConfig {
        endpoint: "https://logs-prod-08.grafana.net/otlp".to_string(),
        protocol: Protocol::Http,
        headers: auth.clone(),
    })
    .with_traces(OtlpConfig {
        endpoint: "https://tempo-prod-08-eu-west-2.grafana.net:443".to_string(),
        protocol: Protocol::Grpc,
        headers: auth.clone(),
    }, 0.1)
    .with_metrics(OtlpConfig {
        endpoint: "https://prometheus-prod-08-eu-west-2.grafana.net/api/v1/otlp".to_string(),
        protocol: Protocol::Http,
        headers: auth,
    }, Duration::from_secs(60))
    .with_profiling(ProfilingConfig {
        endpoint: "https://profiles-prod-008.grafana.net:443".to_string(),
        sample_rate_hz: 100,
        auth_token: Some(api_token.to_string()),
        basic_auth: None,
    })
    .init();
```

### 3. Hybrid (alloy for some, direct cloud for others)

For example: route logs and metrics through a local alloy (so you get
disk-buffering on network glitches) while traces go straight to Tempo Cloud
(skipping the local agent for high-volume signals).

```rust
let alloy = OtlpConfig {
    endpoint: "http://localhost:4317".to_string(),
    protocol: Protocol::Grpc,
    headers: HashMap::new(),
};

Builder::new()
    .service_name("my-app")
    .with_logs(alloy.clone())
    .with_metrics(alloy.clone(), Duration::from_secs(15))
    .with_traces(OtlpConfig {
        endpoint: "https://tempo-prod-08-eu-west-2.grafana.net:443".to_string(),
        protocol: Protocol::Grpc,
        headers: cloud_auth_headers,
    }, 0.1)
    .init();
```

### 4. Logs only

Some workloads (cron jobs, scripts, libraries) only want structured logging.
Skip the rest.

```rust
Builder::new()
    .service_name("my-cron-job")
    .with_logs(OtlpConfig {
        endpoint: "http://localhost:4317".to_string(),
        protocol: Protocol::Grpc,
        headers: HashMap::new(),
    })
    .init();
```

## HTTP middleware (axum)

This crate handles SDK setup. For HTTP server instrumentation you'll typically
add `axum-otel-metrics` and `tower-http`'s `TraceLayer` on top:

```toml
[dependencies]
rust_telemetry = { git = "https://github.com/zygmunt-pawel/rust_telemetry" }
axum = "0.8"
axum-otel-metrics = "0.13"
tower-http = { version = "0.6", features = ["trace"] }
```

```rust
use axum::{routing::get, Router};
use axum_otel_metrics::HttpMetricsLayerBuilder;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() {
    let _guard = rust_telemetry::Builder::new()
        .service_name("my-api")
        .with_logs(/*...*/)
        .with_traces(/*...*/, 0.1)
        .with_metrics(/*...*/, Duration::from_secs(15))
        .init();

    // OTel HTTP metrics (RED: rate, errors, duration) — uses the global
    // MeterProvider which `init()` set up.
    let metrics_layer = HttpMetricsLayerBuilder::new().build();

    let app = Router::new()
        .route("/", get(|| async { "hello" }))
        // route_layer (not layer) so metrics see MatchedPath populated by
        // axum's router. With plain .layer(), http.route would be empty.
        .route_layer(metrics_layer)
        // tower-http TraceLayer — creates a per-request span, propagating
        // trace_id to every event/log inside the handler.
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

You'll get:

- `http.server.request.duration` histogram (with `http.route`, `http.request.method`, `http.response.status_code` attributes) → Prometheus-compatible RED dashboard.
- A root span per request with `trace_id`/`span_id` propagated to every `tracing::info!` / `error!` / etc. inside the handler.

## Notes & limitations

### Pyroscope per-request tagging
Pyroscope's `tag_wrapper()` API is per-OS-thread, but Tokio task-aware. Tasks
migrate between threads on every `.await`, so per-request tags (like
`trace_id`) leak across requests in async runtimes. This crate doesn't try
to attach `trace_id` to profile samples — use Grafana's `tracesToProfiles`
datasource link for time-window-based correlation instead. For low-traffic
apps the time window is precise enough; for high-traffic, you'll see
aggregated profiles per window which is the standard observation pattern.

### OTel exemplars
Rust OTel SDK 0.31 has `exemplars: vec![]` hardcoded in metric aggregation.
Click-from-metric-spike-to-trace-via-exemplar (one of OTel's headline flows)
does not work yet. Will start working automatically once the upstream SDK
adds exemplar reservoir support — no changes needed in your code.

### Pyroscope upstream
This crate uses `pyroscope = "0.5.8"` and `pyroscope_pprofrs = "0.2.10"`.
There's a `pyroscope = "2.0.x"` line on crates.io with a different API,
but `pyroscope_pprofrs 0.2.10` requires `pyroscope ~0.5.7`. Keep them on
the matched line.

### Test isolation
`pprof-rs` installs a global SIGPROF signal handler. If your test suite
calls `Builder::with_profiling(...).init()` from multiple tests in one
process, you need to serialize them (a static `Mutex<()>` works). The
crate's own integration tests demonstrate this.

### mTLS
TLS (https/grpc+tls) is auto-enabled by URL scheme. Mutual TLS (client
certificates) is not yet supported — open an issue if you need it.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
