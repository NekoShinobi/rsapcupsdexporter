//! metrics.rs
//!
//! Turns apcupsd status snapshots into Prometheus metric families.
//!
//! Metrics are built from scratch on every scrape rather than accumulated in a
//! long-lived registry. That makes staleness impossible - a key that stops
//! appearing in the apcupsd output simply stops being emitted - and bounds
//! memory to the current snapshot regardless of how many distinct keys have
//! been seen over the process lifetime.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use log::debug;
use prometheus::proto::MetricFamily;
use prometheus::{GaugeVec, IntGaugeVec, Opts, Registry};

/// Label applied to every metric, identifying which UPS it came from.
pub const TARGET_LABEL: &str = "ups";

/// Keys exposed as labels on the metadata metric instead of as gauges,
/// paired with the label name they map to.
pub const INFO_KEYS: &[(&str, &str)] = &[
    ("APC", "apc"),
    ("HOSTNAME", "hostname"),
    ("UPSNAME", "upsname"),
    ("VERSION", "version"),
    ("CABLE", "cable"),
    ("MODEL", "model"),
    ("UPSMODE", "upsmode"),
    ("DRIVER", "driver"),
    ("APCMODEL", "apcmodel"),
    ("STATUS", "status"),
];

/// Keys whose values are timestamps rather than numbers.
///
/// These previously failed `parse::<f64>()` and were dropped entirely.
pub const DATE_KEYS: &[&str] = &[
    "DATE",
    "STARTTIME",
    "XONBATT",
    "XOFFBATT",
    "LASTSTEST",
    "BATTDATE",
    "END APC",
];

fn is_info_key(key: &str) -> bool {
    INFO_KEYS.iter().any(|(k, _)| *k == key)
}

/// Current wall-clock time as fractional unix seconds.
pub fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The result of one attempt to poll a target.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub stats: BTreeMap<String, String>,
    pub ok: bool,
    pub duration_seconds: f64,
    /// When the most recent attempt finished. `None` if never attempted.
    pub attempted_at: Option<f64>,
    /// When the most recent *successful* attempt finished.
    pub last_success_at: Option<f64>,
}

impl Snapshot {
    /// A target that has been configured but not yet polled.
    pub fn pending() -> Self {
        Self::default()
    }

    pub fn success(stats: BTreeMap<String, String>, duration_seconds: f64) -> Self {
        let now = now_unix();
        Self {
            stats,
            ok: true,
            duration_seconds,
            attempted_at: Some(now),
            last_success_at: Some(now),
        }
    }

    /// A failed attempt. Previous stats are dropped so we never serve values
    /// that look current but are not. The underlying error is logged by the
    /// caller at the point of failure.
    pub fn failure(duration_seconds: f64, previous: Option<&Snapshot>) -> Self {
        Self {
            stats: BTreeMap::new(),
            ok: false,
            duration_seconds,
            attempted_at: Some(now_unix()),
            last_success_at: previous.and_then(|p| p.last_success_at),
        }
    }
}

/// Convert an apcupsd status key into a valid Prometheus metric name.
///
/// apcupsd emits keys containing spaces (`END APC`), which would produce an
/// invalid metric name. Building one used to panic through `unwrap()`; now
/// anything outside `[a-z0-9_]` becomes an underscore.
pub fn sanitize_metric_name(key: &str) -> Option<String> {
    let mut name = String::with_capacity(key.len());
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_lowercase());
        } else {
            name.push('_');
        }
    }
    let name = name.trim_matches('_').to_string();
    if name.is_empty() { None } else { Some(name) }
}

/// Days since the unix epoch for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse an apcupsd timestamp into unix seconds.
///
/// Handles the formats emitted by apcupsd 3.14+:
///   `2024-01-15 10:23:45 -0500`, `2024-01-15 10:23:45 -05:00`,
///   `2024-01-15 10:23:45` (assumed UTC) and `2024-01-15`.
/// Returns `None` for `N/A`, empty values and anything unrecognised.
pub fn parse_timestamp(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("N/A") {
        return None;
    }

    let mut parts = value.split_whitespace();
    let date = parts.next()?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut seconds = days_from_civil(year, month, day) * 86_400;

    if let Some(time) = parts.next() {
        let mut time_parts = time.split(':');
        let hour: i64 = time_parts.next()?.parse().ok()?;
        let minute: i64 = time_parts.next()?.parse().ok()?;
        // Seconds are optional in some apcupsd builds.
        let second: i64 = match time_parts.next() {
            Some(s) => s.parse().ok()?,
            None => 0,
        };
        if time_parts.next().is_some()
            || !(0..24).contains(&hour)
            || !(0..60).contains(&minute)
            || !(0..=60).contains(&second)
        {
            return None;
        }
        seconds += hour * 3600 + minute * 60 + second;

        if let Some(offset) = parts.next() {
            seconds -= parse_utc_offset(offset)?;
        }
    }

    Some(seconds)
}

/// Parse `+HHMM`, `-HHMM`, `+HH:MM` or `Z` into seconds east of UTC.
fn parse_utc_offset(offset: &str) -> Option<i64> {
    if offset.eq_ignore_ascii_case("Z") || offset.eq_ignore_ascii_case("UTC") {
        return Some(0);
    }

    let (sign, rest) = match offset.as_bytes().first()? {
        b'+' => (1, &offset[1..]),
        b'-' => (-1, &offset[1..]),
        _ => return None,
    };

    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    if minutes >= 60 {
        return None;
    }

    Some(sign * (hours * 3600 + minutes * 60))
}

/// Build metric families for the supplied targets.
///
/// The registry is local to this call and dropped on return, so nothing
/// accumulates across scrapes.
pub fn collect(entries: &[(&str, &Snapshot)]) -> Result<Vec<MetricFamily>, prometheus::Error> {
    let registry = Registry::new();

    let up = IntGaugeVec::new(
        Opts::new("apcupsd_up", "Whether the last scrape of apcupsd succeeded"),
        &[TARGET_LABEL],
    )?;
    let duration = GaugeVec::new(
        Opts::new(
            "apcupsd_scrape_duration_seconds",
            "Duration of the last apcupsd scrape",
        ),
        &[TARGET_LABEL],
    )?;
    let last_scrape = GaugeVec::new(
        Opts::new(
            "apcupsd_last_scrape_timestamp_seconds",
            "Unix timestamp of the last apcupsd scrape attempt",
        ),
        &[TARGET_LABEL],
    )?;
    let last_success = GaugeVec::new(
        Opts::new(
            "apcupsd_last_success_timestamp_seconds",
            "Unix timestamp of the last successful apcupsd scrape",
        ),
        &[TARGET_LABEL],
    )?;

    let mut info_labels = vec![TARGET_LABEL];
    info_labels.extend(INFO_KEYS.iter().map(|(_, label)| *label));
    let metadata = IntGaugeVec::new(
        Opts::new("apcupsd_metadata", "APC UPS daemon information"),
        &info_labels,
    )?;

    registry.register(Box::new(up.clone()))?;
    registry.register(Box::new(duration.clone()))?;
    registry.register(Box::new(last_scrape.clone()))?;
    registry.register(Box::new(last_success.clone()))?;
    registry.register(Box::new(metadata.clone()))?;

    // Metric names are shared across targets; each target contributes one
    // child series distinguished by the `ups` label.
    let mut gauges: HashMap<String, GaugeVec> = HashMap::new();

    for &(target, snapshot) in entries {
        up.with_label_values(&[target])
            .set(if snapshot.ok { 1 } else { 0 });

        if let Some(at) = snapshot.attempted_at {
            duration
                .with_label_values(&[target])
                .set(snapshot.duration_seconds);
            last_scrape.with_label_values(&[target]).set(at);
        }
        if let Some(at) = snapshot.last_success_at {
            last_success.with_label_values(&[target]).set(at);
        }

        if snapshot.stats.is_empty() {
            continue;
        }

        let mut label_values: Vec<&str> = Vec::with_capacity(INFO_KEYS.len() + 1);
        label_values.push(target);
        for (key, _) in INFO_KEYS {
            label_values.push(snapshot.stats.get(*key).map(String::as_str).unwrap_or(""));
        }
        metadata.with_label_values(&label_values).set(1);

        for (key, value) in &snapshot.stats {
            if is_info_key(key) {
                continue;
            }

            let Some((name, parsed)) = numeric_metric(key, value) else {
                continue;
            };

            if !gauges.contains_key(&name) {
                let opts = Opts::new(name.clone(), format!("APC UPS {key}"));
                let gauge = match GaugeVec::new(opts, &[TARGET_LABEL]) {
                    Ok(gauge) => gauge,
                    Err(e) => {
                        debug!("skipping metric for key {key}: {e}");
                        continue;
                    }
                };
                if let Err(e) = registry.register(Box::new(gauge.clone())) {
                    debug!("skipping metric {name}: {e}");
                    continue;
                }
                gauges.insert(name.clone(), gauge);
            }

            if let Some(gauge) = gauges.get(&name) {
                gauge.with_label_values(&[target]).set(parsed);
            }
        }
    }

    Ok(registry.gather())
}

/// Map an apcupsd key/value pair to a metric name and value, if it is numeric
/// or a recognised timestamp.
fn numeric_metric(key: &str, value: &str) -> Option<(String, f64)> {
    let base = sanitize_metric_name(key)?;

    if DATE_KEYS.contains(&key) {
        let ts = parse_timestamp(value)?;
        return Some((format!("apcupsd_{base}_timestamp_seconds"), ts as f64));
    }

    let parsed = value.parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some((format!("apcupsd_{base}"), parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{Encoder, TextEncoder};

    fn stats(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Render entries the way the HTTP handler does, so assertions run against
    /// the exposition text a Prometheus server would actually see.
    fn encode(entries: &[(&str, &Snapshot)]) -> String {
        let families = collect(entries).unwrap();
        let mut buf = Vec::new();
        TextEncoder::new().encode(&families, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn has_metric(out: &str, name: &str) -> bool {
        out.lines()
            .any(|l| !l.starts_with('#') && l.starts_with(name))
    }

    #[test]
    fn sanitizes_names_with_spaces() {
        assert_eq!(sanitize_metric_name("END APC").as_deref(), Some("end_apc"));
        assert_eq!(sanitize_metric_name("LINEV").as_deref(), Some("linev"));
        assert_eq!(
            sanitize_metric_name("SELF-TEST").as_deref(),
            Some("self_test")
        );
        assert_eq!(sanitize_metric_name("  ").as_deref(), None);
        assert_eq!(sanitize_metric_name("").as_deref(), None);
    }

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2024, 1, 1), 19723);
    }

    #[test]
    fn parses_timestamps() {
        // 2024-01-15 10:23:45 -0500 == 2024-01-15T15:23:45Z
        assert_eq!(
            parse_timestamp("2024-01-15 10:23:45 -0500"),
            Some(1_705_332_225)
        );
        assert_eq!(
            parse_timestamp("2024-01-15 10:23:45 -05:00"),
            Some(1_705_332_225)
        );
        assert_eq!(parse_timestamp("2024-01-15 15:23:45"), Some(1_705_332_225));
        assert_eq!(
            parse_timestamp("2024-01-15 15:23:45 +0000"),
            Some(1_705_332_225)
        );
        assert_eq!(parse_timestamp("2020-06-01"), Some(1_590_969_600));
        assert_eq!(parse_timestamp("1970-01-01 00:00:00 +0000"), Some(0));
    }

    #[test]
    fn positive_offsets_subtract() {
        // 2024-01-15 10:23:45 +0200 == 2024-01-15T08:23:45Z
        assert_eq!(
            parse_timestamp("2024-01-15 10:23:45 +0200"),
            Some(1_705_307_025)
        );
    }

    #[test]
    fn rejects_non_timestamps() {
        assert_eq!(parse_timestamp("N/A"), None);
        assert_eq!(parse_timestamp("n/a"), None);
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("ONLINE"), None);
        assert_eq!(parse_timestamp("2024-13-01"), None);
        assert_eq!(parse_timestamp("2024-01-32"), None);
        assert_eq!(parse_timestamp("2024-01-15 25:00:00"), None);
        assert_eq!(parse_timestamp("2024-01-15 10:99:00"), None);
        assert_eq!(parse_timestamp("not-a-date"), None);
    }

    #[test]
    fn exports_date_keys_as_timestamps() {
        let snapshot = Snapshot::success(
            stats(&[
                ("BATTDATE", "2020-06-01"),
                ("STARTTIME", "2024-01-15 10:23:45 -0500"),
                ("XONBATT", "N/A"),
            ]),
            0.01,
        );
        let out = encode(&[("ups1:3551", &snapshot)]);

        assert!(out.contains("apcupsd_battdate_timestamp_seconds{ups=\"ups1:3551\"} 1590969600"));
        assert!(out.contains("apcupsd_starttime_timestamp_seconds{ups=\"ups1:3551\"} 1705332225"));
        // N/A produces no series at all rather than a bogus zero.
        assert!(!has_metric(&out, "apcupsd_xonbatt_timestamp_seconds"));
    }

    #[test]
    fn labels_every_metric_with_target() {
        let snapshot = Snapshot::success(stats(&[("LINEV", "120.0")]), 0.01);
        let out = encode(&[("ups1:3551", &snapshot)]);
        assert!(out.contains("apcupsd_linev{ups=\"ups1:3551\"} 120"));
    }

    #[test]
    fn separates_targets_by_label() {
        let a = Snapshot::success(stats(&[("LINEV", "120.0")]), 0.01);
        let b = Snapshot::success(stats(&[("LINEV", "230.5")]), 0.01);
        let out = encode(&[("ups1:3551", &a), ("ups2:3551", &b)]);

        assert!(out.contains("apcupsd_linev{ups=\"ups1:3551\"} 120"));
        assert!(out.contains("apcupsd_linev{ups=\"ups2:3551\"} 230.5"));
    }

    #[test]
    fn disappearing_keys_are_not_retained() {
        let first = Snapshot::success(stats(&[("LINEV", "120.0"), ("ITEMP", "27.4")]), 0.01);
        let out = encode(&[("ups1:3551", &first)]);
        assert!(has_metric(&out, "apcupsd_itemp"));

        // Same target, ITEMP no longer reported by apcupsd.
        let second = Snapshot::success(stats(&[("LINEV", "120.0")]), 0.01);
        let out = encode(&[("ups1:3551", &second)]);
        assert!(has_metric(&out, "apcupsd_linev"));
        assert!(!has_metric(&out, "apcupsd_itemp"));
    }

    #[test]
    fn failed_target_reports_up_zero_without_stats() {
        let snapshot = Snapshot::failure(0.5, None);
        let out = encode(&[("ups1:3551", &snapshot)]);

        assert!(out.contains("apcupsd_up{ups=\"ups1:3551\"} 0"));
        assert!(!has_metric(&out, "apcupsd_linev"));
        assert!(!has_metric(&out, "apcupsd_metadata"));
    }

    #[test]
    fn pending_target_reports_up_zero_and_no_timestamps() {
        let snapshot = Snapshot::pending();
        let out = encode(&[("ups1:3551", &snapshot)]);

        assert!(out.contains("apcupsd_up{ups=\"ups1:3551\"} 0"));
        assert!(!has_metric(&out, "apcupsd_last_scrape_timestamp_seconds"));
        assert!(!has_metric(&out, "apcupsd_last_success_timestamp_seconds"));
    }

    #[test]
    fn failure_preserves_last_success_timestamp() {
        let ok = Snapshot::success(stats(&[("LINEV", "120.0")]), 0.01);
        let failed = Snapshot::failure(15.0, Some(&ok));
        assert_eq!(failed.last_success_at, ok.last_success_at);
        assert!(failed.last_success_at.is_some());
    }

    #[test]
    fn info_keys_become_labels_not_gauges() {
        let snapshot = Snapshot::success(
            stats(&[
                ("STATUS", "ONLINE"),
                ("MODEL", "Smart-UPS 1500"),
                ("LINEV", "120.0"),
            ]),
            0.01,
        );
        let out = encode(&[("ups1:3551", &snapshot)]);

        assert!(!has_metric(&out, "apcupsd_status"));
        assert!(out.contains("status=\"ONLINE\""));
        assert!(out.contains("model=\"Smart-UPS 1500\""));
        assert!(out.contains("ups=\"ups1:3551\""));
    }

    #[test]
    fn key_with_space_does_not_panic() {
        // Previously `GaugeVec::new("apcupsd_end apc").unwrap()` aborted.
        let snapshot = Snapshot::success(stats(&[("END APC", "2024-01-15 10:23:45 -0500")]), 0.01);
        let out = encode(&[("ups1:3551", &snapshot)]);
        assert!(has_metric(&out, "apcupsd_end_apc_timestamp_seconds"));
    }

    #[test]
    fn non_numeric_values_are_skipped() {
        let snapshot = Snapshot::success(
            stats(&[
                ("LASTXFER", "No transfers since turnon"),
                ("STATFLAG", "0x05000008"),
            ]),
            0.01,
        );
        let out = encode(&[("ups1:3551", &snapshot)]);
        assert!(!has_metric(&out, "apcupsd_lastxfer"));
        assert!(!has_metric(&out, "apcupsd_statflag"));
    }

    #[test]
    fn duplicate_sanitized_names_do_not_error() {
        // "FOO BAR" and "FOO_BAR" both sanitize to foo_bar.
        let snapshot = Snapshot::success(stats(&[("FOO BAR", "1"), ("FOO_BAR", "2")]), 0.01);
        let out = encode(&[("ups1:3551", &snapshot)]);
        let count = out
            .lines()
            .filter(|l| !l.starts_with('#') && l.starts_with("apcupsd_foo_bar"))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn rejects_non_finite_values() {
        let snapshot = Snapshot::success(stats(&[("LINEV", "inf"), ("ITEMP", "NaN")]), 0.01);
        let out = encode(&[("ups1:3551", &snapshot)]);
        assert!(!has_metric(&out, "apcupsd_linev"));
        assert!(!has_metric(&out, "apcupsd_itemp"));
    }
}
