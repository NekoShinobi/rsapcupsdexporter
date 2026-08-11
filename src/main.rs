mod apcaccess;
mod metrics;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use actix_web::middleware::Compress;
use actix_web::{App, HttpResponse, HttpServer, web};
use log::{debug, error, info, warn};
use prometheus::proto::MetricFamily;
use prometheus::{Encoder, IntCounterVec, Opts, Registry, TextEncoder};
use tokio::sync::RwLock;
use tokio::time::{Duration, MissedTickBehavior, interval};

use metrics::{Snapshot, TARGET_LABEL};

const DEFAULT_PORT: u16 = 3551;
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// One apcupsd NIS endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    /// `host:port`, used as the value of the `ups` label.
    label: String,
}

impl Target {
    fn new(host: String, port: u16) -> Self {
        let label = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        Self { host, port, label }
    }
}

struct AppState {
    targets: Vec<Target>,
    /// Latest snapshot per target, index-aligned with `targets`.
    ///
    /// Writers swap the whole `Arc` while holding the lock for only the
    /// duration of the assignment; the apcupsd fetch always happens outside
    /// the lock so a slow or hanging UPS can never block a scrape.
    snapshots: Vec<RwLock<Arc<Snapshot>>>,
    /// Counters that must survive across scrapes. Only configured targets are
    /// ever recorded here, so this cannot grow without bound.
    persistent: Registry,
    scrape_errors: IntCounterVec,
    timeout: u64,
}

/// Parse `host`, `host:port`, `[v6addr]` or `[v6addr]:port`.
fn parse_target(spec: &str, default_port: u16) -> Option<Target> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }

    if let Some(rest) = spec.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match tail {
            "" => default_port,
            _ => tail.strip_prefix(':')?.parse().ok()?,
        };
        return Some(Target::new(host.to_string(), port));
    }

    // A bare IPv6 literal contains multiple colons; treat it as a host.
    if let Some((host, port)) = spec.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(':')
    {
        return Some(Target::new(host.to_string(), port.parse().ok()?));
    }

    Some(Target::new(spec.to_string(), default_port))
}

/// Read an environment variable, warning instead of silently falling back when
/// the value is present but unparseable.
fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Err(_) => default,
        Ok(raw) => match raw.trim().parse() {
            Ok(value) => value,
            Err(_) => {
                warn!("invalid value for {name}: {raw:?}, using default");
                default
            }
        },
    }
}

/// Build the target list from `APCUPSD_TARGETS`, falling back to the legacy
/// single-host `APCUPSD_HOST` / `APCUPSD_PORT` pair.
fn configure_targets() -> Vec<Target> {
    let default_port: u16 = env_parse("APCUPSD_PORT", DEFAULT_PORT);

    let raw = std::env::var("APCUPSD_TARGETS").unwrap_or_default();
    let mut targets: Vec<Target> = Vec::new();

    if raw.trim().is_empty() {
        let host = std::env::var("APCUPSD_HOST").unwrap_or_else(|_| "localhost".to_string());
        if let Some(target) = parse_target(&host, default_port) {
            targets.push(target);
        }
    } else {
        for spec in raw.split(',') {
            match parse_target(spec, default_port) {
                Some(target) => targets.push(target),
                None if spec.trim().is_empty() => {}
                None => warn!("ignoring unparseable target {spec:?}"),
            }
        }
    }

    // Duplicate targets would produce duplicate series for the same `ups`
    // label, which Prometheus rejects.
    let mut seen = HashSet::new();
    targets.retain(|t| seen.insert(t.label.clone()));

    targets
}

fn encode_response(families: &[MetricFamily]) -> HttpResponse {
    let mut buffer = Vec::new();
    match TextEncoder::new().encode(families, &mut buffer) {
        Ok(()) => HttpResponse::Ok()
            .content_type(EXPOSITION_CONTENT_TYPE)
            .body(buffer),
        Err(e) => {
            error!("failed to encode metrics: {e}");
            HttpResponse::InternalServerError().body("failed to encode metrics\n")
        }
    }
}

/// `GET /metrics` serves the cached snapshots of every configured target.
///
/// Polling happens in the background, so this never blocks on a slow or
/// unreachable UPS.
async fn metrics_handler(state: web::Data<AppState>) -> HttpResponse {
    let mut snapshots = Vec::with_capacity(state.targets.len());
    for slot in &state.snapshots {
        snapshots.push(slot.read().await.clone());
    }

    let entries: Vec<(&str, &Snapshot)> = state
        .targets
        .iter()
        .zip(snapshots.iter())
        .map(|(target, snapshot)| (target.label.as_str(), snapshot.as_ref()))
        .collect();

    let mut families = match metrics::collect(&entries) {
        Ok(families) => families,
        Err(e) => {
            error!("failed to build metrics: {e}");
            return HttpResponse::InternalServerError().body("failed to build metrics\n");
        }
    };
    families.extend(state.persistent.gather());

    encode_response(&families)
}

/// Fetch one target, timing the attempt. Never returns an error: a failure is
/// itself a reportable observation (`apcupsd_up 0`).
async fn scrape(target: &Target, timeout: u64, previous: Option<&Snapshot>) -> Snapshot {
    let start = Instant::now();
    let result = apcaccess::fetch_stats(&target.host, target.port, timeout, true).await;
    let elapsed = start.elapsed().as_secs_f64();

    match result {
        Ok(stats) => {
            debug!(
                "scraped {} in {elapsed:.3}s ({} keys)",
                target.label,
                stats.len()
            );
            Snapshot::success(stats, elapsed)
        }
        Err(e) => {
            error!("failed to scrape {}: {e}", target.label);
            Snapshot::failure(elapsed, previous)
        }
    }
}

/// One independent poller per target, so a stalled UPS cannot delay the others.
fn spawn_poller(state: Arc<AppState>, index: usize, period: Duration) {
    tokio::spawn(async move {
        let mut ticker = interval(period);
        // Recover from a slow scrape by resuming the cadence rather than
        // firing a burst of catch-up ticks.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            let target = &state.targets[index];
            let previous = state.snapshots[index].read().await.clone();
            let snapshot = scrape(target, state.timeout, Some(&previous)).await;

            if !snapshot.ok {
                state
                    .scrape_errors
                    .with_label_values(&[target.label.as_str()])
                    .inc();
            }

            *state.snapshots[index].write().await = Arc::new(snapshot);
        }
    });
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();

    let targets = configure_targets();
    let port_bind: u16 = env_parse("METRICS_PORT", 9090u16);
    let fetch_interval: u64 = env_parse("INTERVAL", 10u64).max(1);
    let timeout: u64 = env_parse("TIMEOUT", 15u64).max(1);

    if targets.is_empty() {
        error!("no usable apcupsd targets configured; set APCUPSD_TARGETS or APCUPSD_HOST");
        return Err(std::io::Error::other("no apcupsd targets configured"));
    }

    let persistent = Registry::new();
    let scrape_errors = IntCounterVec::new(
        Opts::new(
            "apcupsd_scrape_errors_total",
            "Total number of failed apcupsd scrapes",
        ),
        &[TARGET_LABEL],
    )
    .map_err(std::io::Error::other)?;
    persistent
        .register(Box::new(scrape_errors.clone()))
        .map_err(std::io::Error::other)?;

    // Initialise every configured target's counter so absent series do not
    // break `rate()` queries before the first failure.
    for target in &targets {
        scrape_errors
            .with_label_values(&[target.label.as_str()])
            .reset();
    }

    // The `process` feature was already enabled but the collector was never
    // registered, so process_* metrics were missing entirely.
    #[cfg(target_os = "linux")]
    if let Err(e) = persistent.register(Box::new(
        prometheus::process_collector::ProcessCollector::for_self(),
    )) {
        warn!("could not register process collector: {e}");
    }

    let snapshots = targets
        .iter()
        .map(|_| RwLock::new(Arc::new(Snapshot::pending())))
        .collect();

    let state = Arc::new(AppState {
        targets,
        snapshots,
        persistent,
        scrape_errors,
        timeout,
    });

    info!(
        "polling {} apcupsd target(s) every {fetch_interval}s (timeout {timeout}s): {}",
        state.targets.len(),
        state
            .targets
            .iter()
            .map(|t| t.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Pollers run in the background. The server binds and starts serving
    // immediately, so an unreachable apcupsd yields `apcupsd_up 0` rather than
    // a crash loop that never exposes anything at all.
    let period = Duration::from_secs(fetch_interval);
    for index in 0..state.targets.len() {
        spawn_poller(Arc::clone(&state), index, period);
    }

    let data = web::Data::from(state);

    info!("serving metrics on 0.0.0.0:{port_bind}/metrics");
    HttpServer::new(move || {
        App::new()
            .wrap(Compress::default())
            .app_data(data.clone())
            .service(web::resource("/metrics").route(web::get().to(metrics_handler)))
    })
    .bind(("0.0.0.0", port_bind))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_and_port() {
        let t = parse_target("ups1:3551", DEFAULT_PORT).unwrap();
        assert_eq!(t.host, "ups1");
        assert_eq!(t.port, 3551);
        assert_eq!(t.label, "ups1:3551");
    }

    #[test]
    fn applies_default_port() {
        let t = parse_target("ups1", 3551).unwrap();
        assert_eq!(t.host, "ups1");
        assert_eq!(t.port, 3551);
        assert_eq!(t.label, "ups1:3551");
    }

    #[test]
    fn honours_non_default_port() {
        let t = parse_target("ups1", 4000).unwrap();
        assert_eq!(t.port, 4000);
        assert_eq!(t.label, "ups1:4000");
    }

    #[test]
    fn parses_ipv4() {
        let t = parse_target("192.168.1.100:3551", DEFAULT_PORT).unwrap();
        assert_eq!(t.host, "192.168.1.100");
        assert_eq!(t.port, 3551);
    }

    #[test]
    fn parses_bracketed_ipv6() {
        let t = parse_target("[::1]:3551", DEFAULT_PORT).unwrap();
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 3551);
        assert_eq!(t.label, "[::1]:3551");

        let t = parse_target("[fe80::1]", 3551).unwrap();
        assert_eq!(t.host, "fe80::1");
        assert_eq!(t.port, 3551);
    }

    #[test]
    fn treats_bare_ipv6_as_host() {
        let t = parse_target("::1", 3551).unwrap();
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 3551);
    }

    #[test]
    fn rejects_bad_targets() {
        assert!(parse_target("", DEFAULT_PORT).is_none());
        assert!(parse_target("   ", DEFAULT_PORT).is_none());
        assert!(parse_target("ups1:notaport", DEFAULT_PORT).is_none());
        assert!(parse_target("ups1:99999", DEFAULT_PORT).is_none());
        assert!(parse_target("[]", DEFAULT_PORT).is_none());
    }
}
