use anyhow::{Context, bail};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use xxgate_codex::model_version::{current, parse, update};
use xxgate_core::application::gateway::Gateway;
use xxgate_postgres::PgStore;

const RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    prerelease: bool,
    draft: bool,
}

fn release_version(body: &[u8]) -> anyhow::Result<String> {
    let release: Release = serde_json::from_slice(body)?;
    if release.prerelease || release.draft {
        bail!("Not a stable release");
    }
    let version = release
        .tag_name
        .strip_prefix("rust-v")
        .context("Not a Codex CLI release")?;
    parse(version).context("Invalid stable Codex version")?;
    Ok(version.to_owned())
}

async fn fetch(client: &reqwest::Client) -> anyhow::Result<String> {
    if let Ok(body) = fetch_body(client, RELEASE_URL).await
        && let Ok(version) = release_version(&body)
    {
        return Ok(version);
    }
    // The repository also releases unrelated components. Fall back to a bounded
    // release list when /latest does not identify a stable CLI release.
    let body = fetch_body(
        client,
        "https://api.github.com/repos/openai/codex/releases?per_page=100",
    )
    .await?;
    let releases: Vec<serde_json::Value> = serde_json::from_slice(&body)?;
    releases
        .iter()
        .filter_map(|release| release_version(&serde_json::to_vec(release).ok()?).ok())
        .max_by_key(|version| parse(version))
        .context("No stable Codex release found")
}

async fn fetch_body(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > 1024 * 1024 {
            bail!("Release response exceeds size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub async fn restore(store: &PgStore) -> anyhow::Result<()> {
    if let Some(version) = store.model_discovery_version().await? {
        update(&version);
    }
    Ok(())
}

pub fn start(gateway: Arc<Gateway>, store: Arc<PgStore>) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("XXGate-Codex-Version-Sync")
        .timeout(Duration::from_secs(20))
        .build()?;
    let task_gateway = gateway.clone();
    gateway.tasks.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 3600));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = task_gateway.shutdown.cancelled() => break,
                _ = interval.tick() => {}
            }
            let sync = async {
                let version = fetch(&client).await?;
                if parse(&version) > parse(&current()) {
                    // Persist first so a restart cannot roll back a successful update.
                    store.save_model_discovery_version(&version).await?;
                    update(&version);
                    tracing::info!(%version, "Codex model discovery version updated");
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::select! {
                _ = task_gateway.shutdown.cancelled() => break,
                result = sync => if let Err(error) = result {
                    tracing::warn!(%error, version=%current(), "Codex version sync failed; retaining previous version");
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_official_stable_cli_release_tags_are_accepted() {
        assert_eq!(
            release_version(br#"{"tag_name":"rust-v0.155.1","draft":false,"prerelease":false}"#)
                .unwrap(),
            "0.155.1"
        );
        for body in [
            r#"{"tag_name":"rust-v0.156.0-alpha.1","draft":false,"prerelease":true}"#,
            r#"{"tag_name":"rust-v0.156.0","draft":true,"prerelease":false}"#,
            r#"{"tag_name":"rusty-v8-v1.0.0","draft":false,"prerelease":false}"#,
            r#"{"tag_name":"rust-v0.156.0-alpha.1","draft":false,"prerelease":false}"#,
            r#"{"message":"API rate limit exceeded"}"#,
        ] {
            assert!(release_version(body.as_bytes()).is_err());
        }
    }
}
