use super::ports::Store;
use crate::{
    Error, Result,
    access::{CredentialCipher, GatewayKey},
    accounts::{Account, Credentials, DisableReason},
    audit::{AuditEvent, RequestRecord},
    identity::{IdentityMap, SessionKey, ThreadKey},
    pricing::{Price, value_usage},
    protocol::{
        EncryptedContentRecovery, GatewayRequest, IngressAdapter, ProviderAdapter, RequestKind,
        UpstreamTransport,
    },
    providers::ModelSpec,
    scheduling::{BudgetedBytes, DispatchLease, MemoryBudget, MemoryLease, QueueTicket, Scheduler},
    settings::LiveSettings,
    usage::Usage,
};
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::{Duration, Instant},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

pub struct Gateway {
    pub store: Arc<dyn Store>,
    pub scheduler: Scheduler,
    pub settings: LiveSettings,
    pub memory: MemoryBudget,
    pub ingress: Arc<dyn IngressAdapter>,
    pub provider: Arc<dyn ProviderAdapter>,
    pub transport: Arc<dyn UpstreamTransport>,
    pub cipher: CredentialCipher,
    pub tasks: TaskTracker,
    pub shutdown: CancellationToken,
    pub accepted: AtomicU64,
    pub completed: AtomicU64,
    pub failed: AtomicU64,
    pub mutations: Mutex<()>,
    pub(crate) safety: super::safety::SafetyGuard,
    refresh_locks: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    pub(crate) model_sync_locks: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
    pub(crate) reset_locks: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
}

pub struct ResponseHandle {
    pub id: Uuid,
    pub stream: bool,
    pub bytes: mpsc::Receiver<Bytes>,
    pub completion: oneshot::Receiver<Completion>,
}
pub struct Completion {
    pub headers: http::HeaderMap,
    pub status: u16,
    pub body: Value,
    pub raw_body: Option<Bytes>,
}
struct Execution {
    response_headers: http::HeaderMap,
    ticket: Option<QueueTicket>,
    record: RequestRecord,
    price: Option<Price>,
    finalized: bool,
    terminal_delivered: bool,
    unary: Option<Value>,
    raw_body: Option<Bytes>,
    safety_rejection: Option<Error>,
}

impl Gateway {
    pub async fn new(
        store: Arc<dyn Store>,
        cipher: CredentialCipher,
        ingress: Arc<dyn IngressAdapter>,
        provider: Arc<dyn ProviderAdapter>,
        transport_builder: impl FnOnce(LiveSettings) -> Arc<dyn UpstreamTransport>,
    ) -> Result<Arc<Self>> {
        let settings = LiveSettings::new(store.settings().await?);
        let scheduler = Scheduler::new(
            settings.clone(),
            store.accounts().await?,
            store.active_bindings().await?,
            store.keys().await?,
        );
        let transport = transport_builder(settings.clone());
        Ok(Arc::new(Self {
            store,
            scheduler,
            memory: MemoryBudget::new(settings.clone()),
            settings,
            ingress,
            provider,
            transport,
            cipher,
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            accepted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            mutations: Mutex::new(()),
            safety: Default::default(),
            refresh_locks: Mutex::new(HashMap::new()),
            model_sync_locks: Mutex::new(HashMap::new()),
            reset_locks: Mutex::new(HashMap::new()),
        }))
    }

    pub async fn start(
        self: &Arc<Self>,
        id: Uuid,
        key: GatewayKey,
        request: GatewayRequest,
        body_bytes: usize,
        memory: MemoryLease,
    ) -> Result<ResponseHandle> {
        let model = self
            .store
            .models()
            .await?
            .into_iter()
            .find(|m| m.id == request.model && m.enabled)
            .ok_or_else(|| {
                Error::new(
                    400,
                    "model_not_configured",
                    "The requested model is not enabled in the gateway",
                )
            })?;
        self.provider.validate(&request, &model)?;
        let safety_session = (!request.stateless).then(|| SessionKey {
            key_id: key.id,
            client_session_id: request.identity.session_id.clone(),
        });
        if let Some(session) = &safety_session {
            self.safety.check(session)?;
            if let Some(error) = self.store.safety_rejection(session).await? {
                self.safety.remember(session.clone(), error.clone());
                return Err(super::safety::blocked(error));
            }
        }
        let since = Instant::now();
        let ticket = self.scheduler.enqueue(
            id,
            ThreadKey {
                session: SessionKey {
                    key_id: key.id,
                    client_session_id: request.identity.session_id.clone(),
                },
                client_thread_id: request.identity.thread_id.clone(),
            },
            key.group_id,
            model.upstream.clone(),
            request.client_origin.source,
            since,
        )?;
        let record = RequestRecord {
            client_turn_state: Some(request.client_turn_state.clone()),
            client_origin: Some(request.client_origin.clone()),
            stateless: request.stateless,
            ingress_diagnostics: request.ingress_diagnostics.clone(),
            kind: request.kind,
            compaction: request.compaction.clone(),
            stream: Some(request.stream),
            search_price: None,
            id,
            key_id: key.id,
            group_id: Some(key.group_id),
            client_session_id: if request.stateless {
                String::new()
            } else {
                request.identity.session_id.clone()
            },
            client_thread_id: if request.stateless {
                String::new()
            } else {
                request.identity.thread_id.clone()
            },
            model: request.model.clone(),
            provider: model.upstream.provider.clone(),
            upstream_model: Some(model.upstream.model.clone()),
            response_model: None,
            requested_tier: request.requested_tier.clone(),
            reasoning_effort: request.reasoning_effort.clone(),
            account_id: None,
            binding_id: None,
            binding_generation: None,
            state: "queued".into(),
            created_at: Utc::now(),
            finished_at: None,
            queue_ms: None,
            first_event_ms: None,
            first_content_ms: None,
            total_ms: None,
            upstream_status: None,
            upstream_headers_ms: None,
            upstream_request_id: None,
            error_code: None,
            error_message: None,
            upstream_error: None,
            usage: Usage::default(),
            valuation: None,
            config_version: self.settings.current().version,
            config_versions: vec![self.settings.current().version],
            body_bytes,
            upstream_attempts: 0,
        };
        self.store.begin_request(&record).await?;
        self.accepted.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(1);
        let (done_tx, done_rx) = oneshot::channel();
        let this = Arc::clone(self);
        let stream = request.stream;
        self.tasks.spawn(async move {
            let _memory = memory;
            let cancel = this.shutdown.child_token();
            let mut execution = Execution { response_headers: http::HeaderMap::new(), ticket: Some(ticket), record, price: None, finalized: false, terminal_delivered: false, unary: None, raw_body: None, safety_rejection: None };
            let result = tokio::select! {
                biased;
                _ = tx.closed() => { cancel.cancel(); Err(Error::cancelled()) },
                _ = cancel.cancelled() => Err(Error::new(503, "gateway_shutdown", "The gateway is shutting down")),
                result = this.execute(&request, &model, &mut execution, &tx, &cancel, since) => result,
            };
            cancel.cancel();
            let error = result.err();
            if let Some(session) = &safety_session
                && let Some(rejection) = execution.safety_rejection.as_ref()
                && let Err(save_error) = this.store.save_safety_rejection(session, rejection).await
            {
                this.scheduler.set_paused(true);
                tracing::error!(request_id=%id,code=%save_error.code,"safety rejection persistence failed; dispatch paused");
            }
            this.note_config(&mut execution,this.settings.current().version);
            if !execution.finalized {
                if let Err(e) = this.finalize(&mut execution, error.as_ref(), since).await {
                    this.scheduler.set_paused(true);
                    tracing::error!(request_id = %id, code = %e.code, "request persistence failed; dispatch paused");
                }
            } else if error.is_some() && !execution.terminal_delivered {
                let _ = this.store.append_event(&AuditEvent::new("delivery_interrupted", "gateway", Some(id), execution.record.account_id, json!({"upstream_completed": true}))).await;
            }
            if let Some(error) = &error {
                this.failed.fetch_add(1, Ordering::Relaxed);
                if stream && !execution.terminal_delivered && !tx.is_closed() { let _ = tokio::time::timeout(Duration::from_secs(3), tx.send(this.ingress.failure(id, error))).await; }
            } else { this.completed.fetch_add(1, Ordering::Relaxed); }
            let completion = match error {
                Some(error) => Completion { headers: execution.response_headers, status: if execution.raw_body.is_some() { execution.record.upstream_status.unwrap_or(error.status) } else { error.status.min(599) }, raw_body: execution.raw_body, body: json!({"error":{"type":"gateway_error","code":error.code,"message":error.client_message()},"request_id":id}) },
                None => Completion { headers: execution.response_headers, status: if execution.raw_body.is_some() { execution.record.upstream_status.unwrap_or(200) } else { 200 }, raw_body: execution.raw_body, body: execution.unary.unwrap_or_else(|| json!({"id":id,"status":"completed"})) },
            };
            let _ = done_tx.send(completion);
        });
        Ok(ResponseHandle {
            id,
            stream,
            bytes: rx,
            completion: done_rx,
        })
    }

    fn check_session_safety(&self, e: &Execution) -> Result<()> {
        if !e.record.stateless {
            self.safety.check(&SessionKey {
                key_id: e.record.key_id,
                client_session_id: e.record.client_session_id.clone(),
            })?;
        }
        Ok(())
    }

    fn observe_safety_rejection(&self, e: &mut Execution, error: &Error) {
        if error.code == "upstream_content_policy_violation" && !e.record.stateless {
            self.safety.remember(
                SessionKey {
                    key_id: e.record.key_id,
                    client_session_id: e.record.client_session_id.clone(),
                },
                error.clone(),
            );
            e.safety_rejection = Some(error.clone());
        }
    }

    async fn wait_for_account(
        &self,
        request: &GatewayRequest,
        model: &ModelSpec,
        e: &mut Execution,
        tx: &mpsc::Sender<Bytes>,
        cancel: &CancellationToken,
        since: Instant,
    ) -> Result<DispatchLease> {
        let ticket = match e.ticket.take() {
            Some(ticket) => ticket,
            None => self.scheduler.enqueue(
                e.record.id,
                ThreadKey {
                    session: SessionKey {
                        key_id: e.record.key_id,
                        client_session_id: request.identity.session_id.clone(),
                    },
                    client_thread_id: request.identity.thread_id.clone(),
                },
                e.record.group_id.ok_or_else(Error::storage)?,
                model.upstream.clone(),
                request.client_origin.source,
                since,
            )?,
        };
        let waiting = ticket.wait(cancel);
        tokio::pin!(waiting);
        let mut cfg = self.settings.subscribe();
        let mut last_heartbeat = Instant::now();
        loop {
            let snapshot = cfg.borrow_and_update().clone();
            self.note_config(e, snapshot.version);
            let heartbeat_at =
                last_heartbeat + Duration::from_millis(snapshot.heartbeat_interval_ms);
            tokio::select! {
                biased;
                lease = &mut waiting => return lease,
                _ = cfg.changed() => {},
                _ = tokio::time::sleep_until(heartbeat_at), if request.stream => {
                    self.send(tx, self.ingress.heartbeat(e.record.id),since,true).await?;
                    last_heartbeat = Instant::now();
                }
            }
        }
    }

    async fn execute(
        &self,
        request: &GatewayRequest,
        model: &ModelSpec,
        e: &mut Execution,
        tx: &mpsc::Sender<Bytes>,
        cancel: &CancellationToken,
        since: Instant,
    ) -> Result<()> {
        let (lease, mut ids, mut prepared, rewrite) = loop {
            let mut lease = self
                .wait_for_account(request, model, e, tx, cancel, since)
                .await?;
            self.check_session_safety(e)?;
            if request.stateless {
                lease.confirm_stateless()?;
            } else if lease.needs_commit {
                if let Err(error) = self
                    .store
                    .commit_binding(&lease.binding, lease.expected_generation)
                    .await
                {
                    // Never issue a new generation after an ambiguous commit acknowledgement.
                    match self.store.session_bindings(&lease.binding.session).await {
                        Ok(bindings)
                            if bindings.last().is_some_and(|b| b.id == lease.binding.id) => {}
                        _ => {
                            self.scheduler.set_paused(true);
                            return Err(error);
                        }
                    }
                }
                lease.confirm_binding();
            }
            let mut ids = IdentityMap::new(
                lease.binding.clone(),
                if request.stateless {
                    vec![]
                } else {
                    self.store.mappings(lease.binding.id).await?
                },
            );
            if !request.stateless && lease.binding.generation > 1 {
                for previous in self.store.session_bindings(&lease.binding.session).await? {
                    if previous.id != lease.binding.id {
                        ids.import_legacy_aliases(
                            &previous,
                            &self.store.mappings(previous.id).await?,
                        );
                    }
                }
            }
            let credentials = self.credentials(lease.account.id, false).await?;
            let account = self
                .scheduler
                .account(lease.account.id)
                .ok_or_else(Error::not_found)?;
            if !account.enabled {
                drop(lease);
                continue;
            }
            let current_model = self
                .store
                .models()
                .await?
                .into_iter()
                .find(|m| m.id == model.id && m.enabled)
                .ok_or_else(|| {
                    Error::new(
                        400,
                        "model_disabled",
                        "The model was disabled before dispatch",
                    )
                })?;
            if current_model.upstream != model.upstream {
                return Err(Error::new(
                    409,
                    "model_route_changed",
                    "The model route changed before dispatch",
                ));
            }
            self.provider.validate(request, &current_model)?;
            e.record.account_id = Some(account.id);
            e.record.binding_id = (!request.stateless).then_some(lease.binding.id);
            e.record.binding_generation = (!request.stateless).then_some(lease.binding.generation);
            e.record.queue_ms = Some(since.elapsed().as_millis() as u64);
            let prepared =
                self.provider
                    .prepare(request, &current_model, &account, &credentials, &mut ids)?;
            let pending = ids.take_pending();
            if !request.stateless {
                self.store.save_mappings(ids.binding.id, &pending).await?;
            }
            if request.kind == RequestKind::Search {
                e.record.search_price = Some(self.store.search_price().await?);
            } else {
                e.price = self
                    .store
                    .prices()
                    .await?
                    .into_iter()
                    .find(|p| p.model == model.upstream);
            }
            match lease.begin_send() {
                Ok(()) => {
                    let rewrite = ids.take_request_rewrite();
                    break (lease, ids, prepared, rewrite);
                }
                Err(error) if error.code == "reservation_invalidated" => {
                    drop(lease);
                    continue;
                }
                Err(error) => return Err(error),
            }
        };
        e.record.account_id = Some(lease.account.id);
        e.record.binding_id = (!request.stateless).then_some(lease.binding.id);
        e.record.binding_generation = (!request.stateless).then_some(lease.binding.generation);
        e.record.queue_ms = Some(since.elapsed().as_millis() as u64);
        e.record.state = "inflight".into();
        e.record.upstream_attempts = 1;
        self.store.update_request(&e.record).await?;
        if let Some(rewrite) = rewrite {
            self.store.append_event(&AuditEvent::new("request_rewritten", "gateway", Some(e.record.id), e.record.account_id,
                json!({"binding_id":e.record.binding_id,"stateless":request.stateless,"entries":rewrite.entries,"omitted":rewrite.omitted}))).await?;
        }
        self.store.append_event(&AuditEvent::new("dispatched", "gateway", Some(e.record.id), e.record.account_id, json!({"group_id":lease.binding.group_id,"binding_id":e.record.binding_id,"generation":e.record.binding_generation,"stateless":request.stateless,"queue_ms":e.record.queue_ms,"client_profile_version":lease.account.version,"codex_version":lease.account.profile.codex_version}))).await?;
        let started = Instant::now();
        let mut cfg = self.settings.subscribe();
        let mut sent_attempts = 0;
        'attempt: loop {
            if cancel.is_cancelled() || tx.is_closed() {
                return Err(Error::cancelled());
            }
            if let Err(error) = self.check_session_safety(e) {
                e.record.upstream_attempts = sent_attempts;
                return Err(error);
            }
            sent_attempts += 1;
            let attempt_started = Instant::now();
            let sending = self.transport.send_once(prepared.clone(), cancel.clone());
            tokio::pin!(sending);
            let mut response = loop {
                let snapshot = cfg.borrow_and_update().clone();
                self.note_config(e, snapshot.version);
                tokio::select! {
                    biased;
                    r = &mut sending => break r?,
                    _ = cfg.changed() => {},
                    _ = tokio::time::sleep_until(attempt_started + Duration::from_millis(snapshot.sse_idle_timeout_ms)) => return Err(Error::new(504,"upstream_headers_timeout","Upstream response headers timed out")),
                }
            };
            e.record.upstream_status = Some(response.status);
            e.record.upstream_headers_ms = Some(started.elapsed().as_millis() as u64);
            tracing::info!(request_id=%e.record.id,account_id=%lease.account.id,attempt=e.record.upstream_attempts,status=response.status,headers_ms=e.record.upstream_headers_ms,"upstream headers received");
            e.record.upstream_request_id = response
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 256)
                .map(str::to_owned);
            self.store
                .append_event(&AuditEvent::new(
                    "upstream_attempt_headers",
                    "gateway",
                    Some(e.record.id),
                    e.record.account_id,
                    json!({"attempt":e.record.upstream_attempts,"status":response.status,
                    "headers_ms":attempt_started.elapsed().as_millis() as u64,
                    "upstream_request_id":e.record.upstream_request_id,
                    "turn_state":crate::turn_state::TurnStateHeader::capture(&response.headers)}),
                ))
                .await?;
            let header_observation = self.provider.headers(&response.headers);
            self.apply_quotas(
                lease.account.id,
                lease.account.version,
                &header_observation.quotas,
            )
            .await?;
            if !(200..300).contains(&response.status) {
                let reading = read_limited(&mut response.bytes, 64 * 1024);
                tokio::pin!(reading);
                let body = loop {
                    let snapshot = cfg.borrow_and_update().clone();
                    self.note_config(e, snapshot.version);
                    tokio::select! {
                        biased;
                        _=cfg.changed()=>{},
                        _=tokio::time::sleep_until(attempt_started+Duration::from_millis(snapshot.sse_idle_timeout_ms))=>return Err(Error::new(504,"upstream_error_body_timeout","Upstream error response timed out")),
                        result=&mut reading=>break result?,
                    }
                };
                if e.record.upstream_attempts == 1
                    && let Some(cleanup) = self.provider.recover_encrypted_reasoning(
                        request.kind,
                        &mut prepared,
                        response.status,
                        &body,
                    )?
                {
                    self.begin_recovery(request, model, e, &lease, cleanup, "http_error")
                        .await?;
                    continue 'attempt;
                }
                let (error, disable) = self.provider.http_error(response.status, &body);
                self.observe_safety_rejection(e, &error);
                if request.kind == RequestKind::Search {
                    let mut memory = self.memory.lease();
                    memory.resize(body.len())?;
                    e.raw_body = Some(Bytes::from_owner(BudgetedBytes {
                        bytes: body,
                        lease: memory,
                    }));
                    e.record.usage.source = "search_http_error".into();
                    e.record.usage.complete = true;
                }
                if let Some(reason) = disable {
                    self.disable_observed(lease.account.id, lease.account.version, reason)
                        .await?;
                }
                return Err(error);
            }
            if request.kind == RequestKind::Compact
                && let Some(value) = response.headers.get("x-codex-turn-state")
            {
                e.response_headers
                    .insert("x-codex-turn-state", value.clone());
            }
            if matches!(request.kind, RequestKind::Search | RequestKind::Compact) {
                return self.receive_unary(response.bytes, e, since, started).await;
            }
            let mut decoder = self.provider.decoder();
            let mut buffering = e.record.upstream_attempts == 1
                && self.provider.has_recoverable_encrypted_input(&prepared);
            let mut preamble = Vec::new();
            let mut preamble_bytes = 0usize;
            let mut preamble_memory = self.memory.lease();
            let mut last_heartbeat = Instant::now();
            let mut last_event = Instant::now();
            let mut decode_memory = self.memory.lease();
            loop {
                let snapshot = cfg.borrow_and_update().clone();
                self.note_config(e, snapshot.version);
                let next = tokio::select! {
                    biased;
                    _ = cfg.changed() => continue,
                    _ = tokio::time::sleep_until(last_event + Duration::from_millis(snapshot.sse_idle_timeout_ms)) => return Err(Error::new(504, "upstream_idle_timeout", "Upstream SSE idle timeout")),
                    _ = tokio::time::sleep_until(last_heartbeat + Duration::from_millis(snapshot.heartbeat_interval_ms)), if buffering && request.stream => {
                        self.send(tx, self.ingress.heartbeat(e.record.id), last_event, false).await?;
                        last_heartbeat = Instant::now();
                        continue;
                    },
                    next = response.bytes.next() => next,
                };
                let Some(chunk) = next else {
                    decoder.finish()?;
                    return Err(Error::new(
                        502,
                        "stream_interrupted",
                        "Upstream closed before a terminal event",
                    ));
                };
                let chunk = chunk?;
                decode_memory.resize((decoder.buffered_bytes() + chunk.len()).saturating_mul(3))?;
                let events = decoder.push(
                    &chunk,
                    &mut ids,
                    self.settings.current().sse_event_limit_bytes,
                )?;
                // Latch safety decisions before any await or downstream delivery:
                // cancellation must not discard an already observed rejection.
                for event in &events {
                    if let Some(error) = &event.observation.error {
                        self.observe_safety_rejection(e, error);
                    }
                }
                if !events.is_empty() {
                    last_event = Instant::now();
                    e.record
                        .first_event_ms
                        .get_or_insert(started.elapsed().as_millis() as u64);
                }
                let pending = ids.take_pending();
                if !request.stateless {
                    self.store.save_mappings(ids.binding.id, &pending).await?;
                }
                for event in events {
                    if buffering
                        && let Some(error) = event.observation.encrypted_rejection
                        && let Some(cleanup) = self
                            .provider
                            .recover_encrypted_stream(&mut prepared, error)?
                    {
                        self.begin_recovery(request, model, e, &lease, cleanup, "sse_error")
                            .await?;
                        continue 'attempt;
                    }
                    if buffering {
                        let bytes = self.ingress.encode(&event.document)?;
                        if event.observation.recovery_preamble
                            && preamble.len() < 16
                            && preamble_bytes + bytes.len() <= 64 * 1024
                        {
                            preamble_bytes += bytes.len();
                            preamble_memory.resize(preamble_bytes)?;
                            preamble.push(bytes);
                        } else {
                            buffering = false;
                            if request.stream {
                                for bytes in preamble.drain(..) {
                                    self.send(tx, bytes, last_event, false).await?;
                                }
                            }
                            preamble.clear();
                            preamble_memory.resize(0)?;
                        }
                    }
                    if let Some(model) = event.observation.response_model {
                        e.record.response_model = Some(model);
                    }
                    if let Some(compaction) = &mut e.record.compaction
                        && let Some(observed) = event.observation.compaction_output
                    {
                        compaction.output_observed = Some(observed);
                    }
                    if event.observation.content {
                        e.record
                            .first_content_ms
                            .get_or_insert(started.elapsed().as_millis() as u64);
                    }
                    if let Some(usage) = event.observation.usage {
                        e.record.usage = usage;
                    }
                    self.apply_quotas(
                        lease.account.id,
                        lease.account.version,
                        &event.observation.quotas,
                    )
                    .await?;
                    if let Some(reason) = event.observation.disable_reason {
                        self.disable_observed(lease.account.id, lease.account.version, reason)
                            .await?;
                    }
                    if event.observation.terminal {
                        if !request.stream {
                            e.unary = self.ingress.unary_response(&event.document);
                        }
                        self.finalize(e, event.observation.error.as_ref(), since)
                            .await?;
                        if request.stream {
                            self.send(tx, self.ingress.encode(&event.document)?, last_event, false)
                                .await?;
                        }
                        e.terminal_delivered = true;
                        return match event.observation.error {
                            Some(error) => Err(error),
                            None => Ok(()),
                        };
                    }
                    if request.stream && !buffering {
                        self.send(tx, self.ingress.encode(&event.document)?, last_event, false)
                            .await?;
                    }
                }
                decode_memory.resize(decoder.buffered_bytes().saturating_mul(3))?;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn begin_recovery(
        &self,
        request: &GatewayRequest,
        model: &ModelSpec,
        e: &mut Execution,
        lease: &DispatchLease,
        cleanup: EncryptedContentRecovery,
        source: &str,
    ) -> Result<()> {
        // Keep the legacy event kind so existing history and clients remain readable.
        self.store
            .append_event(&AuditEvent::new(
                "encrypted_reasoning_recovery",
                "gateway",
                Some(e.record.id),
                e.record.account_id,
                json!({"failed_attempt":1,"retry_attempt":2,"status":e.record.upstream_status,
                "source":source,"reason":"invalid_encrypted_content","cleanup":cleanup,
                "binding_id":e.record.binding_id}),
            ))
            .await?;
        let current_model = self
            .store
            .models()
            .await?
            .into_iter()
            .find(|m| m.id == model.id && m.enabled)
            .ok_or_else(|| {
                Error::new(
                    400,
                    "model_disabled",
                    "The model was disabled before recovery",
                )
            })?;
        if current_model.upstream != model.upstream {
            return Err(Error::new(
                409,
                "model_route_changed",
                "The model route changed before recovery",
            ));
        }
        self.provider.validate(request, &current_model)?;
        lease.begin_encrypted_reasoning_recovery()?;
        e.record.upstream_attempts = 2;
        e.record.upstream_status = None;
        e.record.upstream_headers_ms = None;
        e.record.upstream_request_id = None;
        e.record.first_event_ms = None;
        e.record.first_content_ms = None;
        e.record.response_model = None;
        e.record.usage = Usage::default();
        self.store.update_request(&e.record).await
    }

    async fn receive_unary(
        &self,
        mut stream: crate::protocol::ByteStream,
        e: &mut Execution,
        since: Instant,
        started: Instant,
    ) -> Result<()> {
        let compact = e.record.kind == RequestKind::Compact;
        let mut body = BytesMut::new();
        let mut memory = self.memory.lease();
        let mut cfg = self.settings.subscribe();
        loop {
            let snapshot = cfg.borrow_and_update().clone();
            self.note_config(e, snapshot.version);
            let chunk = tokio::select! {
                biased;
                _=cfg.changed()=>continue,
                _=tokio::time::sleep_until(started+Duration::from_millis(snapshot.sse_idle_timeout_ms))=>return Err(Error::new(504, if compact { "compact_timeout" } else { "search_timeout" }, "Upstream JSON response timed out")),
                chunk=stream.next()=>chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk?;
            let size = body.len().saturating_add(chunk.len());
            if size > snapshot.sse_event_limit_bytes {
                return Err(Error::new(
                    502,
                    if compact {
                        "compact_response_too_large"
                    } else {
                        "search_response_too_large"
                    },
                    "Upstream JSON response exceeds the configured limit",
                ));
            }
            memory.resize(size.saturating_mul(16))?;
            if !chunk.is_empty() {
                e.record
                    .first_event_ms
                    .get_or_insert(started.elapsed().as_millis() as u64);
            }
            body.extend_from_slice(&chunk);
        }
        if compact {
            let observation = self.provider.compact_response(&body)?;
            e.record.response_model = observation.response_model;
            if let Some(usage) = observation.usage {
                e.record.usage = usage;
            }
            if let Some(compaction) = &mut e.record.compaction {
                compaction.output_observed = observation.compaction_output;
            }
        } else {
            e.record.usage = self.provider.search_response(&body)?;
        }
        e.record.first_content_ms = Some(started.elapsed().as_millis() as u64);
        self.finalize(e, None, since).await?;
        memory.resize(body.len())?;
        e.raw_body = Some(Bytes::from_owner(BudgetedBytes {
            bytes: body.freeze(),
            lease: memory,
        }));
        e.terminal_delivered = true;
        Ok(())
    }

    async fn send(
        &self,
        tx: &mpsc::Sender<Bytes>,
        bytes: Bytes,
        anchor: Instant,
        queued: bool,
    ) -> Result<()> {
        let mut lease = self.memory.lease();
        lease.grow(bytes.len())?;
        deliver(
            tx,
            Bytes::from_owner(BudgetedBytes { bytes, lease }),
            &self.settings,
            anchor,
            queued,
        )
        .await
    }
    fn note_config(&self, e: &mut Execution, version: i64) {
        if e.record.config_versions.last() != Some(&version) {
            e.record.config_versions.push(version);
        }
    }
    async fn finalize(
        &self,
        e: &mut Execution,
        error: Option<&Error>,
        since: Instant,
    ) -> Result<()> {
        e.record.finished_at = Some(Utc::now());
        e.record.total_ms = Some(since.elapsed().as_millis() as u64);
        e.record.state = error
            .map_or("completed", |error| {
                if error.code == "client_cancelled" {
                    "cancelled"
                } else {
                    "failed"
                }
            })
            .into();
        e.record.error_code = error.map(|v| v.code.clone());
        e.record.error_message = error.map(|v| v.message.clone());
        e.record.upstream_error = error.and_then(|v| v.upstream.clone());
        if let Some(error) = error {
            if let Some(diagnostics) = e.record.ingress_diagnostics.as_mut() {
                diagnostics["failure_stage"] = json!(if error.code == "session_safety_blocked" {
                    "safety_policy"
                } else if e.record.upstream_status.is_some() {
                    "upstream_response"
                } else if e.record.upstream_attempts > 0 {
                    "upstream_transport"
                } else {
                    "queue_or_prepare"
                });
                diagnostics["error"] =
                    json!({"code":error.code,"message":error.message,"status":error.status});
            }
            tracing::warn!(request_id=%e.record.id,account_id=?e.record.account_id,code=%error.code,diagnostics=%e.record.ingress_diagnostics.as_ref().unwrap_or(&serde_json::Value::Null),"request failed");
        }
        e.record.valuation = Some(if e.record.upstream_attempts == 0 {
            e.record.usage = Usage {
                complete: true,
                source: "not_executed".into(),
                ..Usage::default()
            };
            crate::pricing::Valuation {
                status: "not_executed".into(),
                price_version: None,
                cny: Some(rust_decimal::Decimal::ZERO),
                items: vec![],
            }
        } else if e.record.kind == RequestKind::Search {
            crate::pricing::value_search(
                e.record.usage.search_calls,
                e.record.search_price.as_ref(),
            )
        } else {
            value_usage(
                &e.record.usage,
                e.record.requested_tier.as_deref(),
                e.price.as_ref(),
            )
        });
        self.store.finish_request(&e.record).await?;
        tracing::info!(request_id=%e.record.id,account_id=?e.record.account_id,state=%e.record.state,code=?e.record.error_code,total_ms=e.record.total_ms,queue_ms=e.record.queue_ms,upstream_attempts=e.record.upstream_attempts,"request finished");
        e.finalized = true;
        Ok(())
    }

    pub async fn set_enabled(
        &self,
        id: Uuid,
        enabled: bool,
        reason: Option<DisableReason>,
        actor: &str,
    ) -> Result<Account> {
        let _lock = self.mutations.lock().await;
        let current = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        if !enabled {
            self.scheduler.block_account(id);
        }
        match self
            .store
            .set_enabled(id, enabled, reason, current.version, actor)
            .await
        {
            Ok(account) => {
                self.scheduler.update_account(account.clone());
                Ok(account)
            }
            Err(error) => {
                if let Ok(accounts) = self.store.accounts().await {
                    for account in accounts {
                        self.scheduler.update_account(account);
                    }
                } else {
                    self.scheduler.set_paused(true);
                }
                Err(error)
            }
        }
    }
    pub async fn disable_observed(
        &self,
        id: Uuid,
        version: i64,
        reason: DisableReason,
    ) -> Result<()> {
        let _lock = self.mutations.lock().await;
        let current = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        if !current.enabled || current.version != version {
            return Ok(());
        }
        self.scheduler.block_account(id);
        match self
            .store
            .set_enabled(id, false, Some(reason), current.version, "upstream")
            .await
        {
            Ok(account) => {
                self.scheduler.update_account(account);
                Ok(())
            }
            Err(error) => {
                self.scheduler.set_paused(true);
                Err(error)
            }
        }
    }
    pub async fn apply_quotas(
        &self,
        id: Uuid,
        version: i64,
        windows: &[crate::quota::QuotaWindow],
    ) -> Result<()> {
        if windows.is_empty() {
            return Ok(());
        }
        self.store.save_quotas(id, windows).await?;
        if let Some(reason) = windows.iter().find_map(|w| w.disable_reason()) {
            self.disable_observed(id, version, reason).await?;
        }
        Ok(())
    }
    pub async fn credentials(&self, id: Uuid, force: bool) -> Result<Credentials> {
        let lock = self
            .refresh_locks
            .lock()
            .await
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _refresh = lock.lock().await;
        let account = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        let credentials = self
            .cipher
            .decrypt(id, &self.store.credentials(id).await?)?;
        let due = credentials
            .expires_at
            .is_some_and(|at| at <= Utc::now() + chrono::Duration::seconds(120));
        if !force && !due {
            return Ok(credentials);
        }
        if credentials.refresh_token.is_empty() {
            if credentials.expires_at.is_some_and(|at| at <= Utc::now()) {
                self.disable_observed(id, account.version, DisableReason::OauthInvalid)
                    .await?;
                return Err(Error::new(
                    401,
                    "oauth_invalid",
                    "The upstream authorization has expired",
                ));
            }
            return if force {
                Err(Error::invalid("No refresh token is available"))
            } else {
                Ok(credentials)
            };
        }
        let request = self.provider.refresh_request(&account, &credentials)?;
        let cancel = self.shutdown.child_token();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let mut response = self.transport.send_once(request, cancel.clone()).await?;
            let body = read_limited(&mut response.bytes, 128 * 1024).await?;
            self.provider
                .refreshed_credentials(response.status, &body, &credentials, &account)
        })
        .await
        .unwrap_or_else(|_| Err(Error::new(504, "oauth_timeout", "OAuth refresh timed out")));
        cancel.cancel();
        match result {
            Ok(credentials) => {
                credentials.validate()?;
                let _lock = self.mutations.lock().await;
                let updated = self
                    .store
                    .update_credentials(
                        id,
                        &self.cipher.encrypt(id, &credentials)?,
                        credentials.expires_at,
                        account.credential_version,
                    )
                    .await?;
                self.scheduler.update_account(updated);
                Ok(credentials)
            }
            Err(error) => {
                if error.code == "oauth_invalid" {
                    self.disable_observed(id, account.version, DisableReason::OauthInvalid)
                        .await?;
                }
                Err(error)
            }
        }
    }
    pub async fn collect_quotas(&self, id: Uuid) -> Result<Vec<crate::quota::QuotaWindow>> {
        let credentials = self.credentials(id, false).await?;
        let account = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        let request = self.provider.quota_request(&account, &credentials)?;
        let cancel = self.shutdown.child_token();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let mut response = self.transport.send_once(request, cancel.clone()).await?;
            let body = read_limited(&mut response.bytes, 256 * 1024).await?;
            if !(200..300).contains(&response.status) {
                let (error, reason) = self.provider.http_error(response.status, &body);
                if let Some(reason) = reason {
                    self.disable_observed(id, account.version, reason).await?;
                }
                return Err(error);
            }
            let windows = self.provider.quota_response(&body)?;
            self.apply_quotas(id, account.version, &windows).await?;
            Ok(windows)
        })
        .await
        .unwrap_or_else(|_| {
            Err(Error::new(
                504,
                "quota_timeout",
                "Quota collection timed out",
            ))
        });
        cancel.cancel();
        result
    }
}

async fn deliver(
    tx: &mpsc::Sender<Bytes>,
    bytes: Bytes,
    settings: &LiveSettings,
    anchor: Instant,
    queued: bool,
) -> Result<()> {
    let sending = tx.send(bytes);
    tokio::pin!(sending);
    let mut changed = settings.subscribe();
    loop {
        let cfg = changed.borrow_and_update().clone();
        let timeout = if queued {
            cfg.queue_timeout_ms
        } else {
            cfg.sse_idle_timeout_ms
        };
        tokio::select! {
            biased;
            _=changed.changed()=>{},
            _=tokio::time::sleep_until(anchor+Duration::from_millis(timeout))=>return Err(Error::new(504,"downstream_backpressure_timeout","The client did not receive data before the configured deadline")),
            result=&mut sending=>return result.map_err(|_|Error::cancelled()),
        }
    }
}

pub async fn read_limited(stream: &mut crate::protocol::ByteStream, limit: usize) -> Result<Bytes> {
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(Error::new(
                502,
                "upstream_body_too_large",
                "Upstream control response exceeds its size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn slow_client_observes_shortened_deadline() {
        let live = LiveSettings::new(Default::default());
        let (tx, _rx) = mpsc::channel(1);
        tx.send(Bytes::from_static(b"occupied")).await.unwrap();
        let settings = live.clone();
        let started = Instant::now();
        let task = tokio::spawn(async move {
            deliver(&tx, Bytes::from_static(b"next"), &settings, started, false).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut cfg = live.current();
        cfg.sse_idle_timeout_ms = 1000;
        live.publish(cfg);
        assert_eq!(
            task.await.unwrap().unwrap_err().code,
            "downstream_backpressure_timeout"
        );
    }
}
