//! Admission uses measured host/cgroup pressure, never a body-size multiplier.
use crate::{Error, settings::LiveSettings};
use serde::Serialize;
#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
pub struct MemoryDomain {
    pub source: String,
    pub limit_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}
impl MemoryDomain {
    fn pressure(&self) -> f64 {
        self.used_bytes as f64 * 100.0 / self.limit_bytes as f64
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct MemorySample {
    pub domains: Vec<MemoryDomain>,
}
pub trait MemoryProbe: Send + Sync {
    fn sample(&self) -> Result<MemorySample, &'static str>;
}
pub struct SystemMemoryProbe;
impl MemoryProbe for SystemMemoryProbe {
    fn sample(&self) -> Result<MemorySample, &'static str> {
        system_sample()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MemoryStatus {
    pub domains: Vec<MemoryDomain>,
    pub reject_percent: u8,
    pub resume_percent: u8,
    pub blocked: bool,
    pub reading_requests: usize,
    pub reader_limit: usize,
    pub probe_error: Option<String>,
}
pub struct MemoryRejection {
    pub error: Error,
    pub status: MemoryStatus,
}
struct State {
    last: Option<Instant>,
    sample: Option<MemorySample>,
    probe_error: Option<&'static str>,
    blocked: bool,
    readers: usize,
}
struct Inner {
    settings: LiveSettings,
    probe: Arc<dyn MemoryProbe>,
    state: Mutex<State>,
    parser: tokio::sync::Mutex<()>,
}
#[derive(Clone)]
pub struct MemoryAdmission(Arc<Inner>);
impl MemoryAdmission {
    pub fn new(settings: LiveSettings, probe: Arc<dyn MemoryProbe>) -> Self {
        Self(Arc::new(Inner {
            settings,
            probe,
            parser: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                last: None,
                sample: None,
                probe_error: None,
                blocked: false,
                readers: 0,
            }),
        }))
    }
    fn status_locked(&self, state: &mut State, fresh: bool) -> MemoryStatus {
        // Serialize sampling and reader admission so simultaneous arrivals cannot
        // bypass the reader limit. Failed samples never reuse stale healthy data.
        if fresh
            || state
                .last
                .is_none_or(|at| at.elapsed() >= Duration::from_millis(20))
        {
            match self.0.probe.sample() {
                Ok(sample)
                    if !sample.domains.is_empty()
                        && sample
                            .domains
                            .iter()
                            .all(|d| d.limit_bytes > 0 && d.available_bytes <= d.limit_bytes) =>
                {
                    state.sample = Some(sample);
                    state.probe_error = None;
                }
                _ => {
                    state.sample = None;
                    state.probe_error = Some("memory_probe_unavailable");
                }
            }
            state.last = Some(Instant::now());
        }
        let cfg = self.0.settings.current();
        if let Some(sample) = &state.sample {
            if sample
                .domains
                .iter()
                .any(|d| d.pressure() >= f64::from(cfg.memory_reject_percent))
            {
                state.blocked = true;
            } else if sample
                .domains
                .iter()
                .all(|d| d.pressure() <= f64::from(cfg.memory_resume_percent))
            {
                state.blocked = false;
            }
        }
        MemoryStatus {
            domains: state
                .sample
                .as_ref()
                .map(|s| s.domains.clone())
                .unwrap_or_default(),
            reject_percent: cfg.memory_reject_percent,
            resume_percent: cfg.memory_resume_percent,
            blocked: state.blocked || state.probe_error.is_some(),
            reading_requests: state.readers,
            reader_limit: cfg.request_read_concurrency,
            probe_error: state.probe_error.map(str::to_owned),
        }
    }
    fn rejection(status: MemoryStatus, reader_limit: bool) -> Box<MemoryRejection> {
        let error = if status.probe_error.is_some() {
            Error::new(
                503,
                "memory_probe_unavailable",
                "Actual memory usage is unavailable; retry later",
            )
        } else if reader_limit {
            Error::new(
                429,
                "request_read_limit",
                "Too many requests are reading or parsing their bodies; retry later",
            )
        } else {
            Error::new(
                429,
                "memory_pressure",
                "Actual host or container memory usage is above the admission threshold; retry later",
            )
        };
        Box::new(MemoryRejection { error, status })
    }
    pub async fn parser(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.parser.lock().await
    }
    pub fn status(&self) -> MemoryStatus {
        self.status_locked(
            &mut self.0.state.lock().unwrap_or_else(|e| e.into_inner()),
            false,
        )
    }
    pub fn enter(&self) -> Result<ReadingPermit, Box<MemoryRejection>> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let status = self.status_locked(&mut state, true);
        if status.blocked {
            return Err(Self::rejection(status, false));
        }
        if state.readers >= status.reader_limit {
            return Err(Self::rejection(status, true));
        }
        state.readers += 1;
        Ok(ReadingPermit(self.clone()))
    }
    pub fn check(&self, fresh: bool) -> Result<(), Box<MemoryRejection>> {
        let status = self.status_locked(
            &mut self.0.state.lock().unwrap_or_else(|e| e.into_inner()),
            fresh,
        );
        if status.blocked {
            Err(Self::rejection(status, false))
        } else {
            Ok(())
        }
    }
}
pub struct ReadingPermit(MemoryAdmission);
impl Drop for ReadingPermit {
    fn drop(&mut self) {
        let mut state = self.0.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.readers = state.readers.saturating_sub(1);
        state.last = None;
    }
}

fn domain(source: &str, total: u64, available: u64) -> Result<MemoryDomain, &'static str> {
    if total == 0 || available > total {
        return Err("invalid_memory_counter");
    }
    Ok(MemoryDomain {
        source: source.into(),
        limit_bytes: total,
        used_bytes: total - available,
        available_bytes: available,
    })
}
#[cfg(any(target_os = "linux", test))]
fn read(path: &Path) -> Result<String, &'static str> {
    std::fs::read_to_string(path).map_err(|_| "memory_counter_unreadable")
}
#[cfg(any(target_os = "linux", test))]
fn number(path: &Path) -> Result<u64, &'static str> {
    read(path)?
        .trim()
        .parse()
        .map_err(|_| "invalid_memory_counter")
}
#[cfg(any(target_os = "linux", test))]
fn host_linux(text: &str) -> Result<MemoryDomain, &'static str> {
    let kb = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name)
                .then(|| {
                    value
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()?
                        .checked_mul(1024)
                })
                .flatten()
        })
    };
    domain(
        "host",
        kb("MemTotal").ok_or("missing_memtotal")?,
        kb("MemAvailable").ok_or("missing_memavailable")?,
    )
}
#[cfg(any(target_os = "linux", test))]
fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
/// Resolve membership against mount roots, including namespaced mounts. Inspect
/// ancestors too: a parent cgroup can impose a tighter limit than this process.
#[cfg(any(target_os = "linux", test))]
fn cgroup_domains(membership: &str, mountinfo: &str) -> Result<Vec<MemoryDomain>, &'static str> {
    let mut result = vec![];
    for line in membership.lines() {
        let fields: Vec<_> = line.splitn(3, ':').collect();
        if fields.len() != 3 {
            continue;
        }
        let v2 = fields[0] == "0" && fields[1].is_empty();
        if !v2 && !fields[1].split(',').any(|c| c == "memory") {
            continue;
        }
        let mut matched = false;
        for mount in mountinfo.lines() {
            let Some((left, right)) = mount.split_once(" - ") else {
                continue;
            };
            let left: Vec<_> = left.split_whitespace().collect();
            let right: Vec<_> = right.split_whitespace().collect();
            if left.len() < 5
                || right.len() < 3
                || !(v2 && right[0] == "cgroup2"
                    || !v2 && right[0] == "cgroup" && right[2].split(',').any(|c| c == "memory"))
            {
                continue;
            }
            let root = PathBuf::from(unescape_mount(left[3]));
            let mountpoint = PathBuf::from(unescape_mount(left[4]));
            let member = PathBuf::from(fields[2]);
            let relative = if member == Path::new("/") {
                Path::new("")
            } else if let Ok(r) = member.strip_prefix(&root) {
                r
            } else {
                continue;
            };
            if relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("invalid_cgroup_path");
            }
            matched = true;
            let mut path = mountpoint.join(relative);
            if !path.is_dir() {
                return Err("cgroup_path_unavailable");
            }
            loop {
                let limit_path = path.join(if v2 {
                    "memory.max"
                } else {
                    "memory.limit_in_bytes"
                });
                match std::fs::read_to_string(&limit_path) {
                    Ok(raw) if raw.trim() != "max" => {
                        let limit: u64 = raw.trim().parse().map_err(|_| "invalid_cgroup_limit")?;
                        // v1 represents unlimited as a page-aligned LONG_MAX.
                        if limit > 0 && limit < (1u64 << 60) {
                            let current = number(&path.join(if v2 {
                                "memory.current"
                            } else {
                                "memory.usage_in_bytes"
                            }))?;
                            result.push(MemoryDomain {
                                source: if v2 { "cgroup_v2" } else { "cgroup_v1" }.into(),
                                limit_bytes: limit,
                                used_bytes: current,
                                available_bytes: limit.saturating_sub(current),
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("cgroup_counter_unreadable"),
                }
                if path == mountpoint {
                    break;
                }
                if !path.pop() || !path.starts_with(&mountpoint) {
                    return Err("invalid_cgroup_mount");
                }
            }
        }
        if !matched {
            return Err("cgroup_mount_not_found");
        }
    }
    Ok(result)
}
#[cfg(target_os = "linux")]
fn system_sample() -> Result<MemorySample, &'static str> {
    let mut domains = vec![host_linux(&read(Path::new("/proc/meminfo"))?)?];
    domains.extend(cgroup_domains(
        &read(Path::new("/proc/self/cgroup"))?,
        &read(Path::new("/proc/self/mountinfo"))?,
    )?);
    Ok(MemorySample { domains })
}
#[cfg(target_os = "macos")]
fn system_sample() -> Result<MemorySample, &'static str> {
    use std::process::Command;
    let total = Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .map_err(|_| "memory_probe_unavailable")?;
    let total: u64 = std::str::from_utf8(&total.stdout)
        .map_err(|_| "invalid_memory_counter")?
        .trim()
        .parse()
        .map_err(|_| "invalid_memory_counter")?;
    let output = Command::new("/usr/bin/vm_stat")
        .output()
        .map_err(|_| "memory_probe_unavailable")?;
    if !output.status.success() {
        return Err("memory_probe_unavailable");
    }
    let text = String::from_utf8(output.stdout).map_err(|_| "invalid_memory_counter")?;
    let page: u64 = text
        .lines()
        .next()
        .and_then(|s| s.split("page size of ").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .ok_or("invalid_page_size")?;
    let mut pages = 0u64;
    let mut found = 0;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':')
            && ["Pages free", "Pages inactive", "Pages speculative"].contains(&key)
        {
            pages = pages
                .checked_add(
                    value
                        .trim()
                        .trim_end_matches('.')
                        .parse::<u64>()
                        .map_err(|_| "invalid_memory_counter")?,
                )
                .ok_or("invalid_memory_counter")?;
            found += 1;
        }
    }
    if found != 3 {
        return Err("missing_memory_counter");
    }
    Ok(MemorySample {
        domains: vec![domain(
            "host",
            total,
            pages.checked_mul(page).ok_or("invalid_memory_counter")?,
        )?],
    })
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_sample() -> Result<MemorySample, &'static str> {
    Err("unsupported_memory_probe")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::RuntimeSettings;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Probe(AtomicU64);
    impl MemoryProbe for Probe {
        fn sample(&self) -> Result<MemorySample, &'static str> {
            let used = self.0.load(Ordering::SeqCst);
            if used == 999 {
                return Err("unavailable");
            }
            Ok(MemorySample {
                domains: vec![
                    domain("host", 1000, 500)?,
                    domain("cgroup_v2", 100, 100 - used)?,
                ],
            })
        }
    }
    fn setup() -> (MemoryAdmission, Arc<Probe>, LiveSettings) {
        let cfg = LiveSettings::new(RuntimeSettings::default());
        let probe = Arc::new(Probe(AtomicU64::new(50)));
        (MemoryAdmission::new(cfg.clone(), probe.clone()), probe, cfg)
    }
    #[test]
    fn measured_container_pressure_has_hysteresis_and_recovers() {
        let (gate, p, _) = setup();
        let permit = gate.enter().unwrap_or_else(|_| panic!());
        p.0.store(85, Ordering::SeqCst);
        let e = gate.check(true).err().unwrap();
        assert_eq!(e.error.code, "memory_pressure");
        assert_eq!(e.status.domains[1].used_bytes, 85);
        p.0.store(80, Ordering::SeqCst);
        assert!(gate.enter().is_err());
        drop(permit);
        p.0.store(75, Ordering::SeqCst);
        assert!(gate.enter().is_ok());
    }
    #[test]
    fn probe_failure_never_reuses_healthy_sample_and_thresholds_are_hot() {
        let (gate, p, cfg) = setup();
        assert!(gate.check(true).is_ok());
        p.0.store(999, Ordering::SeqCst);
        assert_eq!(
            gate.check(true).err().unwrap().error.code,
            "memory_probe_unavailable"
        );
        p.0.store(70, Ordering::SeqCst);
        assert!(gate.check(true).is_ok());
        let mut next = cfg.current();
        next.memory_reject_percent = 65;
        next.memory_resume_percent = 60;
        cfg.publish(next);
        assert!(gate.check(true).is_err());
        p.0.store(60, Ordering::SeqCst);
        assert!(gate.check(true).is_ok());
    }
    #[test]
    fn reader_cap_is_atomic_and_drop_releases_it() {
        let (gate, _, cfg) = setup();
        let mut next = cfg.current();
        next.request_read_concurrency = 1;
        cfg.publish(next);
        let held = gate.enter().unwrap_or_else(|_| panic!());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let g = &gate;
                scope.spawn(move || {
                    assert_eq!(g.enter().err().unwrap().error.code, "request_read_limit")
                });
            }
        });
        assert_eq!(gate.status().reading_requests, 1);
        drop(held);
        assert!(gate.enter().is_ok());
        assert_eq!(gate.status().reading_requests, 0);
    }
    #[test]
    fn linux_available_memory_includes_reclaimable_memory() {
        let d = host_linux("MemTotal: 1000 kB\nMemFree: 10 kB\nMemAvailable: 400 kB\n").unwrap();
        assert_eq!(d.used_bytes, 600 * 1024);
        assert!(host_linux("MemTotal: 100 kB").is_err());
        assert!(host_linux("MemTotal: 100 kB\nMemAvailable: 200 kB").is_err());
    }
    #[test]
    fn nested_cgroups_and_v1_limits_are_observed() {
        let dir = std::env::temp_dir().join(format!("xxgate-memory-{}", uuid::Uuid::new_v4()));
        let child = dir.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(dir.join("memory.max"), "1000").unwrap();
        std::fs::write(dir.join("memory.current"), "900").unwrap();
        std::fs::write(child.join("memory.max"), "max").unwrap();
        let mount = format!("1 0 0:1 / {} rw - cgroup2 cgroup rw", dir.display());
        let samples = cgroup_domains("0::/child", &mount).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].used_bytes, 900);
        // Namespace root resolves to the visible mount; all finite parents count.
        assert_eq!(cgroup_domains("0::/", &mount).unwrap().len(), 1);
        std::fs::write(child.join("memory.max"), "200").unwrap();
        assert!(cgroup_domains("0::/child", &mount).is_err());
        std::fs::write(child.join("memory.current"), "220").unwrap();
        let samples = cgroup_domains("0::/child", &mount).unwrap();
        assert_eq!(samples[0].used_bytes, 220);
        assert_eq!(samples[0].available_bytes, 0);
        std::fs::write(dir.join("memory.limit_in_bytes"), "9223372036854771712").unwrap();
        std::fs::write(child.join("memory.limit_in_bytes"), "500").unwrap();
        std::fs::write(child.join("memory.usage_in_bytes"), "300").unwrap();
        let mount = format!("1 0 0:1 / {} rw - cgroup cgroup rw,memory", dir.display());
        let samples = cgroup_domains("7:cpu,memory:/child", &mount).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].limit_bytes, 500);
        assert!(cgroup_domains("0::/child", "").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn system_probe_returns_real_memory_without_fabricated_defaults() {
        let sample = SystemMemoryProbe.sample().unwrap();
        assert!(!sample.domains.is_empty());
        assert!(
            sample
                .domains
                .iter()
                .all(|d| d.limit_bytes > 0 && d.available_bytes <= d.limit_bytes)
        );
    }
    #[test]
    fn host_pressure_alone_can_block_unlimited_containers() {
        struct Host;
        impl MemoryProbe for Host {
            fn sample(&self) -> Result<MemorySample, &'static str> {
                Ok(MemorySample {
                    domains: vec![domain("host", 100, 5)?],
                })
            }
        }
        let gate = MemoryAdmission::new(
            LiveSettings::new(RuntimeSettings::default()),
            Arc::new(Host),
        );
        assert_eq!(gate.enter().err().unwrap().error.code, "memory_pressure");
    }
}
