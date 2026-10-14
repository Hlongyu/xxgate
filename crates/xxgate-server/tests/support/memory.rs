use super::*;
use std::sync::atomic::AtomicU64;
use xxgate_core::memory::{MemoryDomain, MemoryProbe, MemorySample};
#[derive(Default)]
pub(super) struct Probe(pub AtomicU64);
impl MemoryProbe for Probe {
    fn sample(&self) -> Result<MemorySample, &'static str> {
        let used = self.0.load(Ordering::SeqCst);
        if used == 999 {
            return Err("probe_failed");
        }
        Ok(MemorySample {
            domains: vec![MemoryDomain {
                source: "cgroup_v2".into(),
                limit_bytes: 1000,
                used_bytes: used * 10,
                available_bytes: 1000 - used * 10,
            }],
        })
    }
}
async fn wait_readers(g: &Gateway, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while g.memory_admission.status().reading_requests != count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
pub(super) async fn verify(
    c: &Client,
    g: &Gateway,
    mock: &Mock,
    probe: &Arc<Probe>,
    a: &str,
    b: &str,
) {
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    let original = c.admin("/settings", "GET", None).await;
    let mut cfg = original.clone();
    cfg["queue_memory_bytes"] = json!(1048576);
    cfg["request_read_concurrency"] = json!(1);
    c.admin("/settings", "PUT", Some(cfg)).await;
    // Legacy virtual budget no longer rejects a healthy request's real payload.
    let large = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .json(&json!({"model":"mock-model","input":"x".repeat(1024*1024+65536)}))
        .send()
        .await
        .unwrap();
    assert_eq!(large.status(), 200);
    large.bytes().await.unwrap();
    wait_readers(g, 0).await;
    let before = mock.calls.load(Ordering::SeqCst);
    for (used, status, code) in [
        (90, 429, "memory_pressure"),
        (80, 429, "memory_pressure"),
        (999, 503, "memory_probe_unavailable"),
    ] {
        probe.0.store(used, Ordering::SeqCst);
        let r = c.response("memory-check", "normal", false, "default").await;
        assert_eq!(r.status(), status);
        assert_eq!(r.headers()["retry-after"], "5");
        let rid = r.headers()["x-request-id"].to_str().unwrap().to_owned();
        assert_eq!(r.json::<Value>().await.unwrap()["error"]["code"], code);
        let d = c.record(&rid).await;
        let r = &d["request"];
        assert_eq!(r["upstream_attempts"], 0);
        assert_eq!(r["body_bytes"], 0);
        assert_eq!(
            r["ingress_diagnostics"]["memory"]["snapshot"]["reject_percent"],
            85
        );
        assert!(!d.to_string().contains("PRIVATE_"));
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    probe.0.store(70, Ordering::SeqCst);
    // Hold one body reader open. A concurrent arrival is rejected before reading.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(c.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let body = json!({"model":"mock-model","input":"hello"}).to_string();
    let head = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        c.key,
        body.len()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    socket.write_all(&body.as_bytes()[..1]).await.unwrap();
    wait_readers(g, 1).await;
    let r = c.response("reader-limit", "normal", false, "default").await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "request_read_limit"
    );
    // Real pressure appearing mid-read rejects before upstream dispatch.
    probe.0.store(95, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(30)).await;
    socket.write_all(&body.as_bytes()[1..]).await.unwrap();
    let mut output = String::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_string(&mut output))
        .await
        .unwrap()
        .unwrap();
    assert!(output.starts_with("HTTP/1.1 429"));
    assert!(output.contains("memory_pressure"));
    drop(socket);
    wait_readers(g, 0).await;
    // Pressure after dispatch must not interrupt an accepted response.
    probe.0.store(50, Ordering::SeqCst);
    let request = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .json(&json!({"model":"mock-model","input":"hello","instructions":"hold"}));
    let sending = tokio::spawn(request.send());
    wait_inflight(g, 1).await;
    wait_readers(g, 0).await;
    probe.0.store(95, Ordering::SeqCst);
    assert!(g.memory_admission.check(true).is_err());
    mock.release.add_permits(1);
    let r = sending.await.unwrap().unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    wait_inflight(g, 0).await;
    probe.0.store(50, Ordering::SeqCst);
    let mut restore = original;
    restore["version"] = c.admin("/settings", "GET", None).await["version"].clone();
    c.admin("/settings", "PUT", Some(restore)).await;
}
