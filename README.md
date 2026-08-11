# rsapcupsdexporter

A lightweight Prometheus exporter for APC UPS devices monitored by apcupsd. Written in Rust for minimal resource usage and maximum performance.

## Disclaimer

This was built with AI-Assistance tools. However, the code complexity and footprint is very tiny, so it should be very easy to understand what it does.

## Overview

This exporter connects to the apcupsd Network Information Server (NIS) to retrieve UPS statistics and exposes them as Prometheus metrics. It automatically discovers all available metrics from your UPS and exports them with appropriate types.

## Features

- **Automatic metric discovery** - All numeric values from apcupsd are exported as gauges
- **Multiple UPSes** - Poll any number of targets, distinguished by a `ups` label
- **Self-monitoring** - `apcupsd_up`, scrape duration and error counters, so a dead UPS is alertable rather than silently stale
- **Info metrics** - UPS metadata (model, version, hostname, etc.) exposed as labels
- **Timestamp metrics** - Battery and transfer dates exported as unix timestamps
- **Periodic updates** - Configurable polling interval for real-time monitoring
- **Minimal footprint** - Static binary built with musl, Docker image under 10MB
- **Production-ready** - Built with actix-web for high performance HTTP serving

## Metrics Exported

Every metric carries a `ups` label identifying the target it came from, in
`host:port` form (for example `ups="192.168.1.100:3551"`).

### Exporter Health

These are always present, even when a UPS is unreachable. Scrape them to alert
on a broken exporter or an unreachable apcupsd.

| Metric | Description |
| -------- | ------------- |
| `apcupsd_up` | `1` if the last scrape of this target succeeded, `0` otherwise |
| `apcupsd_scrape_duration_seconds` | Duration of the last scrape |
| `apcupsd_last_scrape_timestamp_seconds` | When the last scrape was attempted |
| `apcupsd_last_success_timestamp_seconds` | When the last *successful* scrape completed |
| `apcupsd_scrape_errors_total` | Counter of failed scrapes |

Standard `process_*` metrics for the exporter itself are also exported on Linux.

When a scrape fails, that target's UPS metrics are withheld rather than left at
their last known values, so a stale reading can never be mistaken for a current
one. Use `apcupsd_last_success_timestamp_seconds` to see how old the last good
reading is.

### Info Metric

- `apcupsd_metadata` - UPS identification and configuration with labels:
  - `ups`, `apc`, `hostname`, `upsname`, `version`, `cable`, `model`, `upsmode`, `driver`, `apcmodel`, `status`

### Gauge Metrics

All numeric values from apcupsd are exported with the prefix `apcupsd_` in lowercase. Common metrics include:

- `apcupsd_linev` - Line voltage
- `apcupsd_loadpct` - Load percentage
- `apcupsd_bcharge` - Battery charge percentage
- `apcupsd_timeleft` - Estimated runtime remaining (minutes)
- `apcupsd_battv` - Battery voltage
- `apcupsd_itemp` - Internal temperature
- And many more depending on your UPS model

### Timestamp Metrics

Date-valued fields are exported as unix timestamps with a
`_timestamp_seconds` suffix. Values of `N/A` produce no series at all.

- `apcupsd_battdate_timestamp_seconds` - Battery installation date
- `apcupsd_starttime_timestamp_seconds` - When apcupsd started
- `apcupsd_xonbatt_timestamp_seconds` - Last transfer to battery
- `apcupsd_xoffbatt_timestamp_seconds` - Last transfer off battery
- `apcupsd_laststest_timestamp_seconds` - Last self test
- `apcupsd_date_timestamp_seconds`, `apcupsd_end_apc_timestamp_seconds`

Alert on battery age with, for example:

```promql
(time() - apcupsd_battdate_timestamp_seconds) / 86400 > 1095
```

## Configuration

All configuration is done via environment variables:

| Variable | Default | Description |
| ---------- | --------- | ------------- |
| `APCUPSD_TARGETS` | *(unset)* | Comma-separated list of `host` or `host:port` targets to poll |
| `APCUPSD_HOST` | `localhost` | Single apcupsd host. Used only when `APCUPSD_TARGETS` is unset |
| `APCUPSD_PORT` | `3551` | Default port for targets that do not specify one |
| `METRICS_PORT` | `9090` | Port to expose Prometheus metrics on |
| `INTERVAL` | `10` | Polling interval in seconds |
| `TIMEOUT` | `15` | Timeout for apcupsd connections in seconds |
| `RUST_LOG` | `error` | Log level (`error`, `warn`, `info`, `debug`) |

IPv6 targets use bracket notation: `[2001:db8::1]:3551`.

If apcupsd is unreachable at startup the exporter still binds and serves
metrics, reporting `apcupsd_up 0` until the target recovers.

## Endpoints

### `GET /metrics`

Serves the most recent cached readings for every target in `APCUPSD_TARGETS`.
Polling happens in the background on the `INTERVAL` cadence, so this endpoint
responds immediately and never blocks on a slow or unreachable UPS.

## Usage

### Docker Standalone

```bash
docker run -d \
  -e APCUPSD_TARGETS=192.168.1.100:3551,192.168.1.101:3551 \
  -e METRICS_PORT=9090 \
  -e INTERVAL=10 \
  -e TIMEOUT=15 \
  -p 9090:9090 \
  rsapcupsdexporter
```

### Docker Compose

```yaml
services:
  apcupsd-exporter:
    image: rsapcupsdexporter
    container_name: apcupsd-exporter
    environment:
      APCUPSD_TARGETS: 192.168.1.100:3551,192.168.1.101:3551
      METRICS_PORT: 9090
      INTERVAL: 10
      TIMEOUT: 15
    ports:
      - "9090:9090"
    restart: unless-stopped
```

### Binary

```bash
export APCUPSD_TARGETS=192.168.1.100:3551
./rsapcupsdexporter
```

Metrics will be available at `http://localhost:9090/metrics`

## Build

### Standalone

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

### Docker

```bash
docker build -t rsapcupsdexporter .
```

The Dockerfile uses multi-stage builds with musl for a minimal scratch-based image.

### Tests

```bash
cargo test
```

## Prometheus Configuration

The exporter holds the target list; Prometheus scrapes one endpoint and gets
every UPS, separated by the `ups` label.

```yaml
scrape_configs:
  - job_name: 'apcupsd'
    static_configs:
      - targets: ['localhost:9090']
```

## License

See LICENSE file for details.
