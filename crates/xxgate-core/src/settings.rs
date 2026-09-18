use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeSettings {
    pub version: i64,
    pub queue_timeout_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub connect_timeout_ms: u64,
    pub sse_idle_timeout_ms: u64,
    pub queue_capacity: usize,
    pub queue_memory_bytes: usize,
    pub request_body_limit_bytes: usize,
    pub sse_event_limit_bytes: usize,
    pub global_max_inflight: u32,
    pub session_max_inflight: u32,
    pub default_account_concurrency: u32,
    pub quota_poll_interval_secs: u64,
    pub quota_stale_after_secs: u64,
    pub request_retention_days: u32,
    pub audit_retention_days: u32,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            version: 1,
            queue_timeout_ms: 300_000,
            heartbeat_interval_ms: 10_000,
            connect_timeout_ms: 15_000,
            sse_idle_timeout_ms: 300_000,
            queue_capacity: 2000,
            queue_memory_bytes: 512 * 1024 * 1024,
            request_body_limit_bytes: 32 * 1024 * 1024,
            sse_event_limit_bytes: 64 * 1024 * 1024,
            global_max_inflight: 200,
            session_max_inflight: 2,
            default_account_concurrency: 2,
            quota_poll_interval_secs: 300,
            quota_stale_after_secs: 900,
            request_retention_days: 30,
            audit_retention_days: 180,
        }
    }
}

impl RuntimeSettings {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("queue_timeout_ms", self.queue_timeout_ms),
            ("connect_timeout_ms", self.connect_timeout_ms),
            ("sse_idle_timeout_ms", self.sse_idle_timeout_ms),
        ] {
            if !(100..=86_400_000).contains(&value) {
                return Err(Error::invalid(format!(
                    "{name} must be between 100 and 86400000"
                )));
            }
        }
        if !(50..=60_000).contains(&self.heartbeat_interval_ms) {
            return Err(Error::invalid(
                "Heartbeat interval must be between 50 and 60000 ms",
            ));
        }
        if self.queue_capacity == 0
            || self.queue_capacity > 100_000
            || self.global_max_inflight == 0
            || self.global_max_inflight > 10_000
            || self.session_max_inflight == 0
            || self.session_max_inflight > 1000
            || self.default_account_concurrency == 0
            || self.default_account_concurrency > 1000
        {
            return Err(Error::invalid("Invalid concurrency or queue capacity"));
        }
        if self.request_body_limit_bytes < 1024
            || self.request_body_limit_bytes > 256 * 1024 * 1024
            || self.sse_event_limit_bytes < 1024
            || self.sse_event_limit_bytes > 256 * 1024 * 1024
            || self.queue_memory_bytes < 1024 * 1024
            || self.queue_memory_bytes > 16usize * 1024 * 1024 * 1024
        {
            return Err(Error::invalid(
                "Invalid request, event or queue memory limit",
            ));
        }
        if !(30..=86_400).contains(&self.quota_poll_interval_secs)
            || self.quota_stale_after_secs < self.quota_poll_interval_secs
            || self.quota_stale_after_secs > 604_800
        {
            return Err(Error::invalid("Invalid quota collection interval"));
        }
        if self.request_retention_days == 0
            || self.audit_retention_days == 0
            || self.request_retention_days > 3650
            || self.audit_retention_days > 3650
        {
            return Err(Error::invalid("Retention must be between 1 and 3650 days"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct LiveSettings(Arc<watch::Sender<RuntimeSettings>>);

impl LiveSettings {
    pub fn new(settings: RuntimeSettings) -> Self {
        Self(Arc::new(watch::channel(settings).0))
    }
    pub fn current(&self) -> RuntimeSettings {
        self.0.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<RuntimeSettings> {
        self.0.subscribe()
    }
    pub fn publish(&self, settings: RuntimeSettings) {
        self.0.send_replace(settings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_settings_default_session_capacity_and_reject_invalid_limits() {
        let mut settings: RuntimeSettings = serde_json::from_str(r#"{"version":7}"#).unwrap();
        assert_eq!(settings.version, 7);
        assert_eq!(settings.session_max_inflight, 2);
        for limit in [1, 2, 1000] {
            settings.session_max_inflight = limit;
            settings.validate().unwrap();
        }
        for limit in [0, 1001] {
            settings.session_max_inflight = limit;
            assert!(settings.validate().is_err());
        }
    }
}
