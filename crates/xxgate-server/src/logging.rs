use std::sync::OnceLock;
use tracing_subscriber::{
    EnvFilter, Registry, layer::SubscriberExt, reload, util::SubscriberInitExt,
};
use xxgate_core::settings::LogLevel;

static FILTER: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

fn filter(level: LogLevel) -> EnvFilter {
    // Debug applies only to our code, not HTTP/TLS dependencies that may log
    // sensitive transport information. Runtime settings are authoritative.
    let dependencies = if level == LogLevel::Debug {
        "warn"
    } else {
        level.as_str()
    };
    let level = level.as_str();
    EnvFilter::new(format!(
        "{dependencies},xxgate_server={level},xxgate_core={level},xxgate_postgres={level},xxgate_codex={level},xxgate_transport={level}"
    ))
}

pub(crate) fn init() {
    let (layer, handle) = reload::Layer::new(filter(LogLevel::Info));
    tracing_subscriber::registry()
        .with(layer)
        .with(tracing_subscriber::fmt::layer().json())
        .init();
    let _ = FILTER.set(handle);
}

pub(crate) fn set_level(level: LogLevel) {
    if let Some(handle) = FILTER.get()
        && let Err(error) = handle.reload(filter(level))
    {
        tracing::error!(%error, "could not reload log level");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{Layer, layer::Context};

    struct Captured(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            self.0
                .lock()
                .unwrap()
                .push(event.metadata().level().to_string());
        }
    }

    #[test]
    fn reload_changes_logging_without_restarting() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (layer, handle) = reload::Layer::new(filter(LogLevel::Info));
        let subscriber = tracing_subscriber::registry()
            .with(layer)
            .with(Captured(events.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "xxgate_core", "hidden");
            tracing::info!(target: "xxgate_core", "visible");
            handle.reload(filter(LogLevel::Debug)).unwrap();
            tracing::debug!(target: "xxgate_core", "visible");
            tracing::debug!(target: "reqwest", "still hidden");
            handle.reload(filter(LogLevel::Error)).unwrap();
            tracing::warn!(target: "xxgate_core", "hidden");
            tracing::error!(target: "xxgate_core", "visible");
        });
        assert_eq!(*events.lock().unwrap(), ["INFO", "DEBUG", "ERROR"]);
    }
}
