//! One sample per second, bounded in-memory history; no request-content labels
//! and no database writes or process spawning on the scrape path.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use xxgate_core::application::gateway::Gateway;

const HISTORY: usize = 300;
#[derive(Clone, Default)]
pub struct Telemetry(Arc<Mutex<VecDeque<Sample>>>);
#[derive(Clone, Serialize, Default)]
pub struct Sample {
    at_ms: i64,
    cpu_single_core_percent: Option<f64>,
    cpu_capacity_percent: Option<f64>,
    cpu_limit_cores: Option<f64>,
    cpu_throttled_periods_percent: Option<f64>,
    host_cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
    memory_limit_bytes: Option<u64>,
    probe_error: Option<String>,
    inflight: u32,
    queued: usize,
    accepted_per_second: Option<f64>,
    completed_per_second: Option<f64>,
    failed_per_second: Option<f64>,
    sampler_delay_ms: f64,
}
#[derive(Default)]
struct Counters {
    cpu_usec: u64,
    periods: Option<u64>,
    throttled: Option<u64>,
    host_total: u64,
    host_idle: u64,
    cores: f64,
    memory: Option<u64>,
    memory_limit: Option<u64>,
    scope: String,
}
struct Previous {
    at: Instant,
    counters: Option<Counters>,
    requests: [u64; 3],
}
fn rate(current: u64, previous: u64, elapsed: f64) -> Option<f64> {
    (elapsed > 0.0)
        .then(|| current.checked_sub(previous).map(|v| v as f64 / elapsed))
        .flatten()
}
impl Telemetry {
    pub fn snapshot(&self, since: Option<i64>) -> serde_json::Value {
        let history = self.0.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"sample_interval_ms":1000,"history_seconds":300,"scope":"current_instance_cgroup_v2",
            "samples":history.iter().filter(|s| since.is_none_or(|since| s.at_ms > since)).collect::<Vec<_>>()})
    }
    fn push(&self, sample: Sample) {
        let mut history = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if history.len() == HISTORY {
            history.pop_front();
        }
        history.push_back(sample);
    }
    pub fn start(&self, gateway: Arc<Gateway>) {
        let telemetry = self.clone();
        let tasks = gateway.tasks.clone();
        tasks.spawn(async move {
            let mut previous: Option<Previous> = None;
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let scheduled = tokio::select! {
                    _ = gateway.shutdown.cancelled() => break,
                    tick = ticker.tick() => tick,
                };
                let delay = tokio::time::Instant::now()
                    .saturating_duration_since(scheduled)
                    .as_secs_f64()
                    * 1000.0;
                let sampled =
                    tokio::task::spawn_blocking(|| (Instant::now(), read_counters())).await;
                let Ok((at, counters)) = sampled else {
                    break;
                };
                let queue = gateway.scheduler.stats();
                let requests = [
                    gateway.accepted.load(Ordering::Relaxed),
                    gateway.completed.load(Ordering::Relaxed),
                    gateway.failed.load(Ordering::Relaxed),
                ];
                let elapsed = previous
                    .as_ref()
                    .map(|p| at.duration_since(p.at).as_secs_f64());
                let current = counters.as_ref().ok();
                let pair = previous
                    .as_ref()
                    .and_then(|p| p.counters.as_ref())
                    .zip(current)
                    .filter(|(p, c)| p.scope == c.scope);
                let cpu = pair.zip(elapsed).and_then(|((p, c), dt)| {
                    rate(c.cpu_usec, p.cpu_usec, dt).map(|v| v / 10_000.0)
                });
                let throttled = pair.and_then(|(p, c)| {
                    let periods = c.periods?.checked_sub(p.periods?)?;
                    let throttled = c.throttled?.checked_sub(p.throttled?)?;
                    Some(if periods == 0 {
                        0.0
                    } else {
                        100.0 * throttled as f64 / periods as f64
                    })
                });
                let host = pair.and_then(|(p, c)| {
                    let total = c.host_total.checked_sub(p.host_total)?;
                    let idle = c.host_idle.checked_sub(p.host_idle)?;
                    (total > 0).then(|| 100.0 * total.saturating_sub(idle) as f64 / total as f64)
                });
                let request_rate = |index| {
                    previous
                        .as_ref()
                        .zip(elapsed)
                        .and_then(|(p, dt)| rate(requests[index], p.requests[index], dt))
                };
                telemetry.push(Sample {
                    at_ms: chrono::Utc::now().timestamp_millis(),
                    cpu_single_core_percent: cpu,
                    cpu_capacity_percent: cpu.zip(current).map(|(v, c)| v / c.cores),
                    cpu_limit_cores: current.map(|c| c.cores),
                    cpu_throttled_periods_percent: throttled,
                    host_cpu_percent: host,
                    memory_bytes: current.and_then(|c| c.memory),
                    memory_limit_bytes: current.and_then(|c| c.memory_limit),
                    probe_error: counters.as_ref().err().map(|e| e.to_string()),
                    inflight: queue.inflight,
                    queued: queue.queued,
                    accepted_per_second: request_rate(0),
                    completed_per_second: request_rate(1),
                    failed_per_second: request_rate(2),
                    sampler_delay_ms: delay,
                });
                previous = Some(Previous {
                    at,
                    counters: counters.ok(),
                    requests,
                });
            }
        });
    }
}

#[cfg(any(target_os = "linux", test))]
mod linux {
    #[cfg(target_os = "linux")]
    use super::Counters;
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "linux")]
    fn read(path: &Path) -> Result<String, &'static str> {
        std::fs::read_to_string(path).map_err(|_| "cpu_probe_unavailable")
    }
    fn field(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.trim().parse().ok()).flatten()
        })
    }
    fn unescape(s: &str) -> String {
        s.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
    }
    fn scope(membership: &str, mounts: &str) -> Option<(PathBuf, PathBuf)> {
        let member = PathBuf::from(membership.lines().find_map(|l| l.strip_prefix("0::"))?);
        for mount in mounts.lines() {
            let Some((left, right)) = mount.split_once(" - ") else {
                continue;
            };
            if right.split_whitespace().next() != Some("cgroup2") {
                continue;
            }
            let fields: Vec<_> = left.split_whitespace().collect();
            if fields.len() < 5 {
                continue;
            }
            let root = PathBuf::from(unescape(fields[3]));
            let point = PathBuf::from(unescape(fields[4]));
            let relative = if member == Path::new("/") {
                Path::new("")
            } else {
                let Ok(r) = member.strip_prefix(root) else {
                    continue;
                };
                r
            };
            if relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return None;
            }
            return Some((point.join(relative), point));
        }
        None
    }
    fn quota(text: &str) -> Option<f64> {
        let mut fields = text.split_whitespace();
        let quota: f64 = fields.next()?.parse().ok()?;
        let period: f64 = fields.next()?.parse().ok()?;
        (quota.is_finite() && period.is_finite() && quota > 0.0 && period > 0.0)
            .then_some(quota / period)
    }
    fn cpus(text: &str) -> Option<usize> {
        text.trim()
            .split(',')
            .try_fold(0usize, |total, range| {
                let count = if let Some((a, b)) = range.split_once('-') {
                    b.parse::<usize>()
                        .ok()?
                        .checked_sub(a.parse().ok()?)?
                        .checked_add(1)?
                } else {
                    range.parse::<usize>().ok()?;
                    1
                };
                total.checked_add(count)
            })
            .filter(|n| *n > 0)
    }
    #[cfg(target_os = "linux")]
    pub(super) fn sample() -> Result<Counters, &'static str> {
        let (path, root) = scope(
            &read(Path::new("/proc/self/cgroup"))?,
            &read(Path::new("/proc/self/mountinfo"))?,
        )
        .ok_or("cgroup_v2_cpu_unavailable")?;
        let stat = read(&path.join("cpu.stat"))?;
        let host = read(Path::new("/proc/stat"))?;
        let host_fields: Vec<u64> = host
            .lines()
            .next()
            .ok_or("host_cpu_unavailable")?
            .split_whitespace()
            .skip(1)
            .take(8)
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| "host_cpu_unavailable")?;
        if host_fields.len() < 5 {
            return Err("host_cpu_unavailable");
        }
        let mut cores = host
            .lines()
            .filter(|l| {
                l.strip_prefix("cpu")
                    .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()))
            })
            .count() as f64;
        if let Ok(text) = read(&path.join("cpuset.cpus.effective"))
            && let Some(count) = cpus(&text)
        {
            cores = cores.min(count as f64);
        }
        let mut current = path.clone();
        let mut memory_limit = None;
        loop {
            if let Ok(text) = read(&current.join("cpu.max"))
                && let Some(limit) = quota(&text)
            {
                cores = cores.min(limit);
            }
            if let Ok(text) = read(&current.join("memory.max"))
                && let Ok(limit) = text.trim().parse::<u64>()
            {
                memory_limit = Some(memory_limit.map_or(limit, |old: u64| old.min(limit)));
            }
            if current == root {
                break;
            }
            if !current.pop() || !current.starts_with(&root) {
                return Err("cgroup_path_unavailable");
            }
        }
        if cores <= 0.0 {
            return Err("cpu_capacity_unavailable");
        }
        Ok(Counters {
            cpu_usec: field(&stat, "usage_usec").ok_or("cpu_usage_unavailable")?,
            periods: field(&stat, "nr_periods"),
            throttled: field(&stat, "nr_throttled"),
            host_total: host_fields.iter().sum(),
            host_idle: host_fields[3] + host_fields[4],
            cores,
            memory: read(&path.join("memory.current"))
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            memory_limit,
            scope: path.to_string_lossy().into_owned(),
        })
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn resolves_namespaces_and_ancestor_paths() {
            let mounts = "29 23 0:26 /docker/a /sys/fs/cgroup rw - cgroup2 cgroup rw";
            assert_eq!(
                scope("0::/", mounts).unwrap().0,
                Path::new("/sys/fs/cgroup")
            );
            assert_eq!(
                scope("0::/docker/a/child", mounts).unwrap().0,
                Path::new("/sys/fs/cgroup/child")
            );
            assert!(scope("0::/docker/a/../escape", mounts).is_none());
        }
        #[test]
        fn fractional_quotas_and_cpuset_ranges() {
            assert_eq!(quota("75000 100000"), Some(0.75));
            assert_eq!(quota("max 100000"), None);
            assert_eq!(quota("0 0"), None);
            assert_eq!(cpus("0-3,6,8-9"), Some(7));
            assert_eq!(cpus("3-1"), None);
            assert_eq!(
                field("usage_usec 123\nnr_throttled 4\n", "usage_usec"),
                Some(123)
            );
        }
    }
}
#[cfg(target_os = "linux")]
fn read_counters() -> Result<Counters, &'static str> {
    linux::sample()
}
#[cfg(not(target_os = "linux"))]
fn read_counters() -> Result<Counters, &'static str> {
    Err("cgroup_v2_cpu_unavailable")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_and_cursor_are_bounded() {
        let telemetry = Telemetry::default();
        for at_ms in 0..500 {
            telemetry.push(Sample {
                at_ms,
                ..Default::default()
            });
        }
        let all = telemetry.snapshot(None);
        assert_eq!(all["samples"].as_array().unwrap().len(), 300);
        assert_eq!(all["samples"][0]["at_ms"], 200);
        assert_eq!(
            telemetry.snapshot(Some(490))["samples"]
                .as_array()
                .unwrap()
                .len(),
            9
        );
        assert!(all["samples"][0]["cpu_capacity_percent"].is_null());
    }
    #[test]
    fn rates_are_interval_aware_and_counter_resets_are_missing() {
        assert_eq!(
            rate(1_500_000, 0, 2.0).map(|v| v / 10_000.0 / 0.75),
            Some(100.0)
        );
        assert_eq!(rate(5, 10, 1.0), None);
        assert_eq!(rate(10, 0, 0.0), None);
    }
}
