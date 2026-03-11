//! System metrics collection and reporting to Sentry
//!
//! Collects CPU usage, CPU temperature, and memory usage on Linux (Raspberry Pi)
//! and sends them periodically to Sentry as custom events.

use log::{debug, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often to collect and send metrics (in seconds)
const METRICS_INTERVAL_SECS: u64 = 60;

/// System metrics snapshot
#[derive(Debug, Clone)]
pub struct SystemMetrics {
    /// CPU usage percentage (0-100)
    pub cpu_percent: f32,
    /// CPU temperature in Celsius
    pub cpu_temp_c: f32,
    /// Memory usage percentage (0-100)
    pub memory_percent: f32,
    /// Available memory in MB
    pub memory_available_mb: u64,
    /// Total memory in MB
    pub memory_total_mb: u64,
}

impl SystemMetrics {
    /// Collect current system metrics
    #[cfg(target_os = "linux")]
    pub fn collect() -> Option<Self> {
        let cpu_percent = read_cpu_usage().unwrap_or(0.0);
        let cpu_temp_c = read_cpu_temperature().unwrap_or(0.0);
        let (memory_percent, memory_available_mb, memory_total_mb) =
            read_memory_usage().unwrap_or((0.0, 0, 0));

        Some(Self {
            cpu_percent,
            cpu_temp_c,
            memory_percent,
            memory_available_mb,
            memory_total_mb,
        })
    }

    /// Stub for non-Linux platforms (development)
    #[cfg(not(target_os = "linux"))]
    pub fn collect() -> Option<Self> {
        // Return dummy values for local development
        Some(Self {
            cpu_percent: 0.0,
            cpu_temp_c: 0.0,
            memory_percent: 0.0,
            memory_available_mb: 0,
            memory_total_mb: 0,
        })
    }

    /// Send metrics to Sentry as a custom event
    pub fn send_to_sentry(&self) {
        use sentry::protocol::{Event, Level};
        use std::collections::BTreeMap;

        // Create event with metrics
        let mut extra: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        extra.insert(
            "cpu_percent".to_string(),
            serde_json::json!(self.cpu_percent),
        );
        extra.insert("cpu_temp_c".to_string(), serde_json::json!(self.cpu_temp_c));
        extra.insert(
            "memory_percent".to_string(),
            serde_json::json!(self.memory_percent),
        );
        extra.insert(
            "memory_available_mb".to_string(),
            serde_json::json!(self.memory_available_mb),
        );
        extra.insert(
            "memory_total_mb".to_string(),
            serde_json::json!(self.memory_total_mb),
        );

        let mut event = Event::new();
        event.level = Level::Info;
        event.message = Some(format!(
            "System metrics: CPU {:.1}% @ {:.1}°C, Memory {:.1}%",
            self.cpu_percent, self.cpu_temp_c, self.memory_percent
        ));
        event.extra = extra;

        // Add tags for filtering/grouping in Sentry
        sentry::configure_scope(|scope| {
            scope.set_tag("metric_type", "system_health");
        });

        sentry::capture_event(event);

        debug!(
            "Sent metrics: CPU {:.1}% @ {:.1}°C, Mem {:.1}%",
            self.cpu_percent, self.cpu_temp_c, self.memory_percent
        );
    }
}

/// Start the metrics collection background thread
pub fn start_metrics_thread(stop_flag: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        info!(
            "Metrics collection started (interval: {}s)",
            METRICS_INTERVAL_SECS
        );

        loop {
            // Check if we should stop
            if stop_flag.load(Ordering::SeqCst) {
                info!("Metrics collection stopped");
                break;
            }

            // Sleep for the interval, checking stop flag periodically
            for _ in 0..(METRICS_INTERVAL_SECS * 10) {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }

            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            // Collect and send metrics
            if let Some(metrics) = SystemMetrics::collect() {
                metrics.send_to_sentry();

                // Also add as breadcrumb for context in errors
                sentry::add_breadcrumb(sentry::Breadcrumb {
                    category: Some("metrics".into()),
                    message: Some(format!(
                        "CPU {:.1}% @ {:.1}°C, Mem {:.1}%",
                        metrics.cpu_percent, metrics.cpu_temp_c, metrics.memory_percent
                    )),
                    level: sentry::Level::Info,
                    ..Default::default()
                });
            }
        }
    });
}

// =============================================================================
// Linux-specific metric collection
// =============================================================================

/// Read CPU temperature from thermal zone (Raspberry Pi)
#[cfg(target_os = "linux")]
fn read_cpu_temperature() -> Option<f32> {
    let temp_str = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let millidegrees: i32 = temp_str.trim().parse().ok()?;
    Some(millidegrees as f32 / 1000.0)
}

/// Read raw CPU stats from /proc/stat
#[cfg(target_os = "linux")]
fn read_cpu_stats_raw() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let cpu_line = stat.lines().next()?;

    if !cpu_line.starts_with("cpu ") {
        return None;
    }

    let values: Vec<u64> = cpu_line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();

    if values.len() < 4 {
        return None;
    }

    // user + nice + system + idle + iowait + irq + softirq
    let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
    let total: u64 = values.iter().take(7).sum();

    Some((idle, total))
}

/// Read CPU usage percentage
#[cfg(target_os = "linux")]
fn read_cpu_usage() -> Option<f32> {
    // Take two samples to calculate CPU usage
    let (idle1, total1) = read_cpu_stats_raw()?;
    std::thread::sleep(Duration::from_millis(100));
    let (idle2, total2) = read_cpu_stats_raw()?;

    let idle_delta = idle2.saturating_sub(idle1);
    let total_delta = total2.saturating_sub(total1);

    if total_delta == 0 {
        return Some(0.0);
    }

    let usage = 100.0 * (1.0 - (idle_delta as f32 / total_delta as f32));
    Some(usage.clamp(0.0, 100.0))
}

/// Read memory usage from /proc/meminfo
#[cfg(target_os = "linux")]
fn read_memory_usage() -> Option<(f32, u64, u64)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;

    let mut total_kb: u64 = 0;
    let mut available_kb: u64 = 0;

    for line in meminfo.lines() {
        if line.starts_with("MemTotal:") {
            total_kb = parse_meminfo_value(line)?;
        } else if line.starts_with("MemAvailable:") {
            available_kb = parse_meminfo_value(line)?;
        }
    }

    if total_kb == 0 {
        return None;
    }

    let used_kb = total_kb.saturating_sub(available_kb);
    let percent = 100.0 * (used_kb as f32 / total_kb as f32);

    Some((percent, available_kb / 1024, total_kb / 1024))
}

/// Parse a meminfo line like "MemTotal:        1234567 kB"
#[cfg(target_os = "linux")]
fn parse_meminfo_value(line: &str) -> Option<u64> {
    line.split_whitespace().nth(1)?.parse().ok()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_collect() {
        let metrics = SystemMetrics::collect();
        assert!(metrics.is_some());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_cpu_temperature_reading() {
        // This test only makes sense on a real Linux system
        if std::path::Path::new("/sys/class/thermal/thermal_zone0/temp").exists() {
            let temp = read_cpu_temperature();
            assert!(temp.is_some());
            let temp = temp.unwrap();
            // Temperature should be reasonable (0-100°C)
            assert!(temp >= 0.0 && temp <= 100.0);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_cpu_usage_reading() {
        let usage = read_cpu_usage();
        assert!(usage.is_some());
        let usage = usage.unwrap();
        assert!(usage >= 0.0 && usage <= 100.0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_memory_usage_reading() {
        let mem = read_memory_usage();
        assert!(mem.is_some());
        let (percent, available, total) = mem.unwrap();
        assert!(percent >= 0.0 && percent <= 100.0);
        assert!(total > 0);
        assert!(available <= total);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_metrics_interval_reasonable() {
        // Interval should be at least 10 seconds to avoid spamming
        assert!(METRICS_INTERVAL_SECS >= 10);
    }
}
