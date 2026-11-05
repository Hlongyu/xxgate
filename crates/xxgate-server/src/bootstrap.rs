use crate::http::AppState;
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use sqlx::Connection;
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
    crate::logging::init();
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
    let role = env::var("XXGATE_ROLE").ok();
    if role
        .as_deref()
        .is_some_and(|r| !["worker", "router"].contains(&r))
    {
        bail!("XXGATE_ROLE must be worker or router");
    }
    let node_id = env::var("XXGATE_NODE_ID").unwrap_or_else(|_| "default".into());
    let store = Arc::new(PgStore::open(&database).await?.for_node(&node_id)?);
    let mut instance_guard = store.single_instance_lock().await?;
    store.check_schema().await?;
    let cluster = if let Some(role) = &role {
        let url =
            env::var("XXGATE_NODE_URL").context("XXGATE_NODE_URL is required in cluster mode")?;
        let parsed = url::Url::parse(&url).context("Invalid XXGATE_NODE_URL")?;
        if !["http", "https"].contains(&parsed.scheme())
            || parsed.host_str().is_none()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            bail!(
                "XXGATE_NODE_URL must be an HTTP(S) origin without credentials, path, query or fragment"
            );
        }
        if role == "router" {
            use xxgate_core::application::ports::AccountStore;
            if !store.accounts().await?.is_empty() {
                bail!("Router node ID already owns accounts; choose a separate node ID");
            }
        }
        let token = env::var("XXGATE_CLUSTER_TOKEN").context("XXGATE_CLUSTER_TOKEN is required")?;
        let boot = uuid::Uuid::new_v4();
        let cluster = crate::cluster::Cluster::new(store.clone(), role == "router", token, boot)?;
        store.register_node(role, &url, boot).await?;
        Some(cluster)
    } else {
        None
    };
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
    crate::logging::set_level(gateway.settings.current().log_level);
    let validation_accounts = if cluster.as_ref().is_some_and(|c| c.router) {
        store.all_accounts().await?
    } else {
        gateway.scheduler.accounts()
    };
    for account in validation_accounts {
        if gateway
            .cipher
            .decrypt(account.id, &gateway.store.credentials(account.id).await?)
            .is_err()
        {
            bail!("Master key cannot decrypt stored account credentials");
        }
    }
    let mut state = AppState::new(gateway.clone(), allow_http, secure_cookies);
    state.cluster = cluster;
    if !state.cluster.as_ref().is_some_and(|c| c.router) {
        crate::model_version::start(gateway.clone(), store.clone())?;
    }
    // A lost advisory-lock connection must stop this process before another
    // process using the same node ID starts dispatching accounts.
    let lock_gateway = gateway.clone();
    gateway.tasks.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! { _=lock_gateway.shutdown.cancelled()=>break, _=interval.tick()=>{} }
            if !matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    sqlx::query("SELECT 1").execute(&mut instance_guard)
                )
                .await,
                Ok(Ok(_))
            ) {
                tracing::error!("node lock connection lost");
                lock_gateway.shutdown.cancel();
                break;
            }
        }
        let _ = instance_guard.close().await;
    });
    crate::workers::start(state.clone());
    let listener = tokio::net::TcpListener::bind(address).await?;
    if let Some(cluster) = &state.cluster {
        cluster.reload(&state).await?;
        cluster.publish(&state).await?;
    }
    crate::cluster::start(state.clone());
    tracing::info!(%address, interrupted, "XXGate started");
    let shutdown = gateway.shutdown.clone();
    let signal = async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {}, _=shutdown.cancelled()=>{} }
        }
        #[cfg(not(unix))]
        {
            tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=shutdown.cancelled()=>{} }
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
