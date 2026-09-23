use crate::http::AppState;
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{env, net::SocketAddr, sync::Arc};
use xxgate_codex::{ingress::ResponsesIngress, provider::CodexProvider};
use xxgate_core::{
    access::{CredentialCipher, hash_password},
    application::{
        gateway::Gateway,
        ports::{AccessStore, RequestStore},
    },
};
use xxgate_postgres::PgStore;
use xxgate_transport::HttpTransport;

pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xxgate=info,xxgate_server=info,xxgate_core=info".into()),
        )
        .json()
        .init();
    let database = env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let key = STANDARD
        .decode(
            env::var("XXGATE_MASTER_KEY")
                .context("XXGATE_MASTER_KEY (32 bytes, base64) is required")?,
        )
        .context("Invalid master key encoding")?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("Master key must contain exactly 32 bytes"))?;
    let address: SocketAddr = env::var("XXGATE_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()?;
    let allow_http = env::var("XXGATE_ALLOW_HTTP_UPSTREAM").is_ok_and(|s| s == "1");
    let secure_cookies =
        env::var("XXGATE_SECURE_COOKIES").map_or(!address.ip().is_loopback(), |s| s != "0");
    let store = Arc::new(PgStore::connect(&database).await?);
    let _instance_guard = store.single_instance_lock().await?;
    if store.admin_password_hash().await?.is_none() {
        let password = env::var("XXGATE_ADMIN_PASSWORD")
            .context("XXGATE_ADMIN_PASSWORD is required on first start (at least 12 characters)")?;
        store.initialize_admin(&hash_password(&password)?).await?;
    }
    let interrupted = store.reconcile_interrupted().await?;
    crate::model_version::restore(&store).await?;
    let gateway = Gateway::new(
        store.clone(),
        CredentialCipher::new(&key),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |settings| Arc::new(HttpTransport::new(settings, allow_http)),
    )
    .await?;
    // Fail startup before accepting traffic if the master key is not the one that encrypted the database.
    for account in gateway.scheduler.accounts() {
        if gateway
            .cipher
            .decrypt(account.id, &gateway.store.credentials(account.id).await?)
            .is_err()
        {
            bail!("Master key cannot decrypt stored account credentials");
        }
    }
    let state = AppState::new(gateway.clone(), allow_http, secure_cookies);
    crate::model_version::start(gateway.clone(), store)?;
    crate::workers::start(state.clone());
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, interrupted, "XXGate started");
    let shutdown = gateway.shutdown.clone();
    let signal = async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        shutdown.cancel();
    };
    axum::serve(listener, crate::http::router(state))
        .with_graceful_shutdown(signal)
        .await?;
    gateway.shutdown.cancel();
    gateway.tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(35), gateway.tasks.wait())
        .await
        .context("Shutdown exceeded 35 seconds")?;
    Ok(())
}
