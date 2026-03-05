# rsapcupsdexporter

A lightweight Prometheus exporter for APC UPS devices monitored by apcupsd. Written in Rust for minimal resource usage and maximum performance.

## Disclaimer

This was built with AI-Assistance tools. However, the code complexity and footprint is very tiny, so it should be very easy to understand what it does.

## Overview

This exporter connects to the apcupsd Network Information Server (NIS) to retrieve UPS statistics and exposes them as Prometheus metrics. It automatically discovers all available metrics from your UPS and exports them with appropriate types.

## Features

- **Automatic metric discovery** - All numeric values from apcupsd are exported as gauges
- **Info metrics** - UPS metadata (model, version, hostname, etc.) exposed as labels
- **Periodic updates** - Configurable polling interval for real-time monitoring
- **Minimal footprint** - Static binary built with musl, Docker image under 10MB
- **Production-ready** - Built with actix-web for high performance HTTP serving

## Metrics Exported

### Info Metric

- `apcupsd_info` - UPS identification and configuration with labels:
  - `apc`, `hostname`, `upsname`, `version`, `cable`, `model`, `upsmode`, `driver`, `apcmodel`, `status`

### Gauge Metrics

All numeric values from apcupsd are exported with the prefix `apcupsd_` in lowercase. Common metrics include:

- `apcupsd_linev` - Line voltage
- `apcupsd_loadpct` - Load percentage
- `apcupsd_bcharge` - Battery charge percentage
- `apcupsd_timeleft` - Estimated runtime remaining
- `apcupsd_battv` - Battery voltage
- `apcupsd_itemp` - Internal temperature
- And many more depending on your UPS model

## Configuration

All configuration is done via environment variables:

| Variable | Default | Description |
| ---------- | --------- | ------------- |
| `APCUPSD_HOST` | `localhost` | Hostname or IP of the apcupsd server |
| `APCUPSD_PORT` | `3551` | Port of the apcupsd NIS |
| `METRICS_PORT` | `8080` | Port to expose Prometheus metrics on |
| `INTERVAL` | `10` | Polling interval in seconds |
| `TIMEOUT` | `15` | Timeout for apcupsd connections in seconds |

## Usage

### Docker Standalone

```bash
docker run -d \
  -e APCUPSD_HOST=192.168.1.100 \
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
      APCUPSD_HOST: 192.168.1.100
      METRICS_PORT: 9090
      INTERVAL: 10
      TIMEOUT: 15
    ports:
      - "9090:9090"
    restart: unless-stopped
```

### Binary

```bash
export APCUPSD_HOST=192.168.1.100
./rsapcupsdexporter
```

Metrics will be available at `http://localhost:8080/metrics`

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

## Prometheus Configuration

Add this job to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'apcupsd'
    static_configs:
      - targets: ['localhost:9090']
```

## License

See LICENSE file for details.
