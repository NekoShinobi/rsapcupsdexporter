mod apcaccess;

use std::sync::{Arc, Mutex};
use tokio::time::{interval, Duration};

use actix_web::middleware::Compress;
use actix_web::{web, App, HttpResponse, HttpServer, Result};
use log::{debug, info};
use prometheus::{Encoder, GaugeVec, IntGaugeVec, Opts, Registry, TextEncoder};

pub struct AppState {
    pub registry: Registry,
    pub info_gauge: IntGaugeVec,
    pub gauges: Arc<Mutex<std::collections::HashMap<String, GaugeVec>>>,
    pub stats: std::collections::BTreeMap<String, String>,
}

pub async fn metrics_handler(state: web::Data<Arc<Mutex<AppState>>>) -> Result<HttpResponse> {
    let state = state.lock().unwrap();
    let encoder = TextEncoder::new();
    let metric_families = state.registry.gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();

    Ok(HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .body(buffer))
}

fn update_metrics(state: &mut AppState) {
    // Update info gauge with labels
    state.info_gauge.reset();
    state.info_gauge
        .with_label_values(&[
            &state.stats.get("APC").cloned().unwrap_or_default(),
            &state.stats.get("HOSTNAME").cloned().unwrap_or_default(),
            &state.stats.get("UPSNAME").cloned().unwrap_or_default(),
            &state.stats.get("VERSION").cloned().unwrap_or_default(),
            &state.stats.get("CABLE").cloned().unwrap_or_default(),
            &state.stats.get("MODEL").cloned().unwrap_or_default(),
            &state.stats.get("UPSMODE").cloned().unwrap_or_default(),
            &state.stats.get("DRIVER").cloned().unwrap_or_default(),
            &state.stats.get("APCMODEL").cloned().unwrap_or_default(),
            &state.stats.get("STATUS").cloned().unwrap_or_default(),
        ])
        .set(1);

    // Update numeric metrics as gauges
    let mut gauges = state.gauges.lock().unwrap();

    for (key, value) in &state.stats {
        // Skip the tag keys that are already in the info metric
        if matches!(key.as_str(), "APC" | "HOSTNAME" | "UPSNAME" | "VERSION" | "CABLE" | "MODEL" | "UPSMODE" | "DRIVER" | "APCMODEL" | "STATUS") {
            continue;
        }

        // Try to parse as f64
        if let Ok(numeric_value) = value.parse::<f64>() {
            let metric_name = format!("apcupsd_{}", key.to_lowercase());

            // Get or create the gauge for this metric
            let gauge = gauges.entry(metric_name.clone()).or_insert_with(|| {
                let opts = Opts::new(metric_name.clone(), format!("APC UPS {}", key));
                let gauge_vec = GaugeVec::new(opts, &[]).unwrap();
                state.registry.register(Box::new(gauge_vec.clone())).unwrap();
                gauge_vec
            });

            gauge.with_label_values(&[] as &[&str]).set(numeric_value);
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {

    env_logger::init();
    let apcupsd_host = std::env::var("APCUPSD_HOST").unwrap_or_else(|_| "localhost".to_string());
    let apcupsd_port: u16 = std::env::var("APCUPSD_PORT")
        .unwrap_or_else(|_| "3551".to_string())
        .parse()
        .unwrap_or(3551);
    let port_bind: u16 = std::env::var("METRICS_PORT")
        .unwrap_or_else(|_| "9090".to_string())
        .parse()
        .unwrap_or(9090);
    let fetch_interval: u64 = std::env::var("INTERVAL")
        .unwrap_or_else(|_| "10".to_string())
        .parse()
        .unwrap_or(10);
    let timeout: u64 = std::env::var("TIMEOUT")
        .unwrap_or_else(|_| "15".to_string())
        .parse()
        .unwrap_or(15);

    // Initial fetch
    debug!("Fetching initial APC UPS stats from {}:{}", apcupsd_host, apcupsd_port);
    let stats = apcaccess::fetch_stats(&apcupsd_host, apcupsd_port, timeout, true)
        .expect("Failed to fetch initial APC UPS stats");
    debug!("Fetched stats: {:?}", stats);
    info!("Successfully fetched initial APC UPS stats");

    // Create registry and metrics
    let registry = Registry::new();

    // Create info gauge with all label names (using _metadata suffix to avoid info type confusion)
    let info_opts = Opts::new("apcupsd_metadata", "APC UPS daemon information");
    let info_gauge = IntGaugeVec::new(
        info_opts,
        &["apc", "hostname", "upsname", "version", "cable", "model", "upsmode", "driver", "apcmodel", "status"]
    ).unwrap();
    registry.register(Box::new(info_gauge.clone())).unwrap();

    let state = Arc::new(Mutex::new(AppState {
        registry,
        info_gauge,
        gauges: Arc::new(Mutex::new(std::collections::HashMap::new())),
        stats: stats.clone(),
    }));

    // Initialize metrics
    {
        let mut state_guard = state.lock().unwrap();
        update_metrics(&mut state_guard);
    }

    // Spawn background task to fetch stats periodically
    let state_clone = Arc::clone(&state);
    let host_clone = apcupsd_host.clone();

    debug!("Starting background task to fetch APC UPS stats every {} seconds", fetch_interval);
    tokio::spawn(async move {
        let mut interval_timer = interval(Duration::from_secs(fetch_interval));
        loop {
            interval_timer.tick().await;

            match apcaccess::fetch_stats(&host_clone, apcupsd_port, timeout, true) {
                Ok(new_stats) => {
                    let mut state_guard = state_clone.lock().unwrap();
                    state_guard.stats = new_stats;
                    update_metrics(&mut state_guard);
                }
                Err(e) => {
                    eprintln!("Failed to fetch APC UPS stats: {}", e);
                }
            }
        }
    });
    info!("Started background task to fetch APC UPS stats every {} seconds", fetch_interval);

    let state = web::Data::new(state);

    debug!("Starting HTTP server on 0.0.0.0:{}", port_bind);
    HttpServer::new(move || {
        App::new()
            .wrap(Compress::default())
            .app_data(state.clone())
            .service(web::resource("/metrics").route(web::get().to(metrics_handler)))
    })
    .bind(("0.0.0.0", port_bind))?
    .run()
    .await
}
