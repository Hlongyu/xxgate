use crate::http::AppState;
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{env, net::SocketAddr, sync::Arc};
use xxgate_codex::{ingress::ResponsesIngress, provider::CodexProvider};
use xxgate_core::{
    access::{CredentialCipher, hash_password},
    application::{
        gateway::Gateway,
        ports::{AccessStore, AccountStore, IdentityStore, RequestStore, SettingsStore},
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
    store.check_schema().await?;
    // Validate decryption before waiting for the incumbent. A bad candidate
    // must fail before the deployment tool asks a healthy worker to drain.
    for account in store.accounts().await? {
        CredentialCipher::new(&key).decrypt(account.id, &store.credentials(account.id).await?)?;
    }
    // Verify the candidate can decode all state needed by Gateway::new before
    // advertising standby. Promotion reloads it after the incumbent drains.
    store.settings().await?;
    store.keys().await?;
    store.active_bindings().await?;
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
        Some(cluster)
    } else {
        None
    };
    // Bind once. The standby control router and the active router share the
    // same listener, so promotion never races rebinding the candidate port.
    let listener = Arc::new(tokio::net::TcpListener::bind(address).await?);
    let wait_secs: u64 = env::var("XXGATE_WAIT_FOR_NODE_LOCK_SECS")
        .unwrap_or_else(|_| "0".into())
        .parse()?;
    let instance_guard = if wait_secs == 0 {
        store.single_instance_lock().await?
    } else {
        let candidate = cluster
            .clone()
            .filter(|c| !c.router)
            .context("Standby requires a cluster worker")?;
        let mut connection = store.standby_lock().await?;
        store.check_schema().await?;
        let stop = tokio_util::sync::CancellationToken::new();
        let app = axum::Router::new().route("/internal/deployment", axum::routing::get({
            let candidate=candidate.clone();
            move |headers: axum::http::HeaderMap| {
                let candidate=candidate.clone();
                async move {
                    if !candidate.authenticated(&headers) { return (axum::http::StatusCode::UNAUTHORIZED,axum::Json(serde_json::json!({"error":"internal_auth_required"}))); }
                    (axum::http::StatusCode::OK,axum::Json(serde_json::json!({"protocol":1,"phase":"standby","role":"worker","node_id":candidate.store.node_id(),"boot_id":candidate.boot_id})))
                }
            }
        })).fallback(||async {axum::http::StatusCode::SERVICE_UNAVAILABLE});
        let server = tokio::spawn({
            let listener = listener.clone();
            let stop = stop.clone();
            async move {
                axum::serve(SharedListener(listener), app)
                    .with_graceful_shutdown(stop.cancelled_owned())
                    .await
            }
        });
        let activation = tokio::time::timeout(std::time::Duration::from_secs(wait_secs), async {
            loop {
                if store.activate_standby(&mut connection).await? {
                    return Ok::<_, xxgate_core::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;
        stop.cancel();
        server.await??;
        activation.context("Timed out waiting for worker ownership")??;
        connection
    };
    store.check_schema().await?;
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
    *state.deployment.ownership.lock().await = Some(instance_guard);
    // Keep ownership through response draining AND task finalization. Planned
    // handoff closes it explicitly; a broken connection always fails closed.
    let ownership_state = state.clone();
    let ownership_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! { _=ownership_state.deployment.stop.cancelled()=>break, _=interval.tick()=>{} }
            let mut ownership = ownership_state.deployment.ownership.lock().await;
            let Some(connection) = ownership.as_mut() else {
                break;
            };
            if !matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    sqlx::query("SELECT 1").execute(connection)
                )
                .await,
                Ok(Ok(_))
            ) {
                tracing::error!("node lock connection lost");
                ownership_state.gateway.shutdown.cancel();
                ownership_state.deployment.stop.cancel();
                break;
            }
        }
    });
    crate::workers::start(state.clone());
    if let Some(cluster) = &state.cluster {
        // Publish the new boot only after state loading and bind succeed.
        cluster.reload(&state).await?;
        cluster
            .register(&state, &env::var("XXGATE_NODE_URL")?)
            .await?;
    }
    crate::cluster::start(state.clone());
    tracing::info!(%address, interrupted, "XXGate started");
    let shutdown = gateway.shutdown.clone();
    let deployment = state.deployment.clone();
    let signal = async move {
        let failed = async {
            shutdown.cancelled().await;
            if matches!(deployment.phase(), "transferring" | "forwarding" | "failed") {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(failed);
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {}, _=&mut failed=>{}, _=deployment.stop.cancelled()=>{} }
        }
        #[cfg(not(unix))]
        {
            tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=&mut failed=>{}, _=deployment.stop.cancelled()=>{} }
        }
        deployment.stop.cancel();
        shutdown.cancel();
    };
    axum::serve(SharedListener(listener), crate::http::router(state.clone()))
        .with_graceful_shutdown(signal)
        .await?;
    state.deployment.stop.cancel();
    gateway.shutdown.cancel();
    gateway.tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(35), gateway.tasks.wait())
        .await
        .context("Shutdown exceeded 35 seconds")?;
    ownership_task.await?;
    state.deployment.release().await?;
    Ok(())
}

/// Read-only release check, safe while the current version serves traffic.
pub async fn preflight() -> anyhow::Result<()> {
    let store = PgStore::open(&env::var("DATABASE_URL")?).await?;
    store.check_schema().await?;
    let key: [u8; 32] = STANDARD
        .decode(env::var("XXGATE_MASTER_KEY")?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Master key must contain exactly 32 bytes"))?;
    let cipher = CredentialCipher::new(&key);
    for account in store.all_accounts().await? {
        cipher.decrypt(account.id, &store.credentials(account.id).await?)?;
    }
    println!("Schema and encrypted credentials are compatible");
    Ok(())
}

struct SharedListener(Arc<tokio::net::TcpListener>);
impl axum::serve::Listener for SharedListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok(connection) => return connection,
                Err(error) => {
                    tracing::warn!(%error, "listener accept failed");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}
