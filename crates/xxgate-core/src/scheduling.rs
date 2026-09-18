use crate::{
    Error, Result,
    access::GatewayKey,
    accounts::Account,
    clients::ClientSource,
    identity::{Binding, SessionKey, ThreadKey},
    settings::LiveSettings,
    types::ModelRef,
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::{
    sync::Notify,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub struct Scheduler(Arc<Inner>);
struct Inner {
    state: Mutex<State>,
    changed: Notify,
    settings: LiveSettings,
}
#[derive(Default)]
struct State {
    accounts: HashMap<Uuid, Account>,
    keys: HashMap<Uuid, GatewayKey>,
    blocked_keys: HashSet<Uuid>,
    blocked: HashSet<Uuid>,
    bindings: HashMap<SessionKey, Binding>,
    queues: HashMap<SessionKey, VecDeque<Waiter>>,
    order: VecDeque<SessionKey>,
    inflight: HashMap<Uuid, u32>,
    active_sessions: HashMap<SessionKey, SessionActivity>,
    leases: HashMap<Uuid, u8>,
    served: HashMap<Uuid, u64>,
    paused: bool,
}
#[derive(Default)]
struct SessionActivity {
    threads: HashMap<String, Uuid>,
    // A new binding must be persisted before another thread can use it.
    binding_pending: Option<Uuid>,
}
#[derive(Clone)]
struct Waiter {
    source: ClientSource,
    id: Uuid,
    thread_id: String,
    group_id: Uuid,
    model: ModelRef,
    since: Instant,
}

#[derive(Debug, Serialize)]
pub struct QueueStats {
    pub queued: usize,
    pub inflight: u32,
    pub paused: bool,
    pub accounts: Vec<AccountRuntime>,
    pub requests: Vec<QueuedRequest>,
}
#[derive(Debug, Serialize)]
pub struct AccountRuntime {
    pub id: Uuid,
    pub inflight: u32,
    pub queued: usize,
    pub max_inflight: u32,
}
#[derive(Debug, Serialize)]
pub struct QueuedRequest {
    pub request_id: Uuid,
    pub session_id: String,
    pub thread_id: String,
    pub session_inflight: usize,
    pub session_max_inflight: u32,
    pub account_id: Option<Uuid>,
    pub wait_ms: u64,
}

impl State {
    fn check_eligibility(
        &self,
        session: &SessionKey,
        group_id: Uuid,
        model: &ModelRef,
        source: ClientSource,
    ) -> Result<()> {
        self.check_key(session.key_id, group_id)?;
        if let Some(binding) = self.bindings.get(session) {
            if !binding.compatible(model) {
                return Err(Error::new(
                    409,
                    "provider_mismatch",
                    "A conversation cannot migrate across providers or access types",
                ));
            }
            if let Some(a) = self
                .accounts
                .get(&binding.account_id)
                .filter(|a| a.enabled && a.group_ids.contains(&group_id) && a.accepts(source))
                && !self.blocked.contains(&a.id)
            {
                if !a.supports(model) {
                    return Err(Error::new(
                        409,
                        "bound_account_model_unsupported",
                        "The bound account does not support this model",
                    ));
                }
                return Ok(());
            }
        }
        let eligible = |a: &&Account| {
            a.enabled
                && !self.blocked.contains(&a.id)
                && a.group_ids.contains(&group_id)
                && a.supports(model)
        };
        if !self
            .accounts
            .values()
            .filter(eligible)
            .any(|a| a.accepts(source))
        {
            if self.accounts.values().any(|a| eligible(&a)) {
                return Err(Error::new(
                    403,
                    "client_source_not_allowed",
                    "No account in this key's group allows the detected client source",
                ));
            }
            return Err(Error::new(
                503,
                "no_available_account",
                "No enabled account in this key's group can serve the requested model",
            ));
        }
        Ok(())
    }

    fn check_key(&self, id: Uuid, group_id: Uuid) -> Result<()> {
        if self.paused {
            return Err(Error::storage());
        }
        if self.blocked_keys.contains(&id) {
            return Err(Error::new(
                503,
                "key_updating",
                "The key is being updated; retry after the change completes",
            ));
        }
        let key = self.keys.get(&id).filter(|k| k.enabled).ok_or_else(|| {
            Error::new(
                401,
                "invalid_api_key",
                "The gateway key is disabled or unavailable",
            )
        })?;
        if key.group_id != group_id {
            return Err(Error::new(
                409,
                "key_group_changed",
                "The key's group changed before dispatch; submit a new request",
            ));
        }
        Ok(())
    }
}

impl Scheduler {
    pub fn new(
        settings: LiveSettings,
        accounts: Vec<Account>,
        bindings: Vec<Binding>,
        keys: Vec<GatewayKey>,
    ) -> Self {
        let state = State {
            accounts: accounts.into_iter().map(|a| (a.id, a)).collect(),
            keys: keys.into_iter().map(|k| (k.id, k)).collect(),
            bindings: bindings
                .into_iter()
                .map(|b| (b.session.clone(), b))
                .collect(),
            ..State::default()
        };
        Self(Arc::new(Inner {
            state: Mutex::new(state),
            changed: Notify::new(),
            settings,
        }))
    }
    pub fn account(&self, id: Uuid) -> Option<Account> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accounts
            .get(&id)
            .cloned()
    }
    pub fn accounts(&self) -> Vec<Account> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accounts
            .values()
            .cloned()
            .collect()
    }
    pub fn update_account(&self, account: Account) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.blocked.remove(&account.id);
        state.accounts.insert(account.id, account);
        drop(state);
        self.wake();
    }
    pub fn update_key(&self, key: GatewayKey) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.blocked_keys.remove(&key.id);
        s.keys.insert(key.id, key);
        drop(s);
        self.wake();
    }
    pub fn block_key(&self, id: Uuid) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blocked_keys
            .insert(id);
        self.wake();
    }
    pub fn block_account(&self, id: Uuid) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blocked
            .insert(id);
        self.wake();
    }
    pub fn set_paused(&self, value: bool) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .paused = value;
        self.wake();
    }
    pub fn wake(&self) {
        self.0.changed.notify_waiters();
    }
    /// Model visibility is scoped by key group, independent of temporary capacity.
    pub fn supports_group_model(
        &self,
        group_id: Uuid,
        model: &ModelRef,
        source: ClientSource,
    ) -> bool {
        let s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.accounts
            .values()
            .any(|a| a.group_ids.contains(&group_id) && a.supports(model) && a.accepts(source))
    }
    pub fn enqueue(
        &self,
        id: Uuid,
        thread: ThreadKey,
        group_id: Uuid,
        model: ModelRef,
        source: ClientSource,
        since: Instant,
    ) -> Result<QueueTicket> {
        let session = thread.session;
        let settings = self.0.settings.current();
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.paused {
            return Err(Error::storage());
        }
        state.check_eligibility(&session, group_id, &model, source)?;
        if state.queues.values().map(VecDeque::len).sum::<usize>() >= settings.queue_capacity {
            return Err(Error::new(
                429,
                "queue_full",
                "The queue has reached its configured capacity",
            ));
        }
        if !state.queues.contains_key(&session) {
            state.order.push_back(session.clone());
        }
        state
            .queues
            .entry(session.clone())
            .or_default()
            .push_back(Waiter {
                source,
                id,
                thread_id: thread.client_thread_id,
                group_id,
                model,
                since,
            });
        drop(state);
        self.wake();
        Ok(QueueTicket {
            scheduler: self.clone(),
            id,
            session,
            since,
        })
    }
    fn try_reserve(&self, id: Uuid) -> Result<Option<DispatchLease>> {
        let cfg = self.0.settings.current();
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.paused {
            return Err(Error::storage());
        }
        let (session, waiter) = s
            .queues
            .iter()
            .find_map(|(session, queue)| queue.iter().find(|w| w.id == id).map(|w| (session, w)))
            .ok_or_else(Error::not_found)?;
        s.check_eligibility(session, waiter.group_id, &waiter.model, waiter.source)?;
        if s.inflight.values().sum::<u32>() >= cfg.global_max_inflight {
            return Ok(None);
        }
        let selected = s.order.iter().find_map(|session| {
            let active = s.active_sessions.get(session);
            if active.is_some_and(|a| {
                a.threads.len() >= cfg.session_max_inflight as usize || a.binding_pending.is_some()
            }) {
                return None;
            }
            let mut seen_threads = HashSet::new();
            s.queues
                .get(session)?
                .iter()
                .enumerate()
                .find_map(|(position, waiter)| {
                    // Preserve FIFO within a thread without blocking other threads
                    // behind that thread's next request.
                    if !seen_threads.insert(&waiter.thread_id)
                        || active.is_some_and(|a| a.threads.contains_key(&waiter.thread_id))
                        || s.check_eligibility(
                            session,
                            waiter.group_id,
                            &waiter.model,
                            waiter.source,
                        )
                        .is_err()
                        || waiter.since.elapsed() >= Duration::from_millis(cfg.queue_timeout_ms)
                    {
                        return None;
                    }
                    let previous = s.bindings.get(session);
                    if previous.is_some_and(|b| s.blocked.contains(&b.account_id)) {
                        return None;
                    }
                    let bound = previous
                        .and_then(|b| s.accounts.get(&b.account_id))
                        .filter(|a| {
                            a.enabled
                                && a.group_ids.contains(&waiter.group_id)
                                && a.accepts(waiter.source)
                        });
                    let account = if let Some(a) = bound {
                        (a.supports(&waiter.model)
                            && s.inflight.get(&a.id).copied().unwrap_or(0) < a.max_inflight)
                            .then_some(a)
                    } else {
                        s.accounts
                            .values()
                            .filter(|a| {
                                a.enabled
                                    && a.group_ids.contains(&waiter.group_id)
                                    && !s.blocked.contains(&a.id)
                                    && a.supports(&waiter.model)
                                    && a.accepts(waiter.source)
                                    && s.inflight.get(&a.id).copied().unwrap_or(0) < a.max_inflight
                            })
                            .min_by_key(|a| {
                                (
                                    s.inflight.get(&a.id).copied().unwrap_or(0),
                                    s.served.get(&a.id).copied().unwrap_or(0),
                                    a.id,
                                )
                            })
                    }?;
                    // Drain every lease of the old generation before migrating,
                    // including a key's group change on a shared account.
                    if active.is_some()
                        && previous.is_none_or(|b| {
                            b.account_id != account.id || b.group_id != waiter.group_id
                        })
                    {
                        return None;
                    }
                    Some((
                        session.clone(),
                        waiter.clone(),
                        position,
                        account.clone(),
                        previous.cloned(),
                    ))
                })
        });
        let Some((session, waiter, position, account, previous)) = selected else {
            return Ok(None);
        };
        if waiter.id != id {
            return Ok(None);
        }
        let needs_commit = previous
            .as_ref()
            .is_none_or(|b| b.account_id != account.id || b.group_id != waiter.group_id);
        let generation = previous.as_ref().map_or(0, |b| b.generation);
        let binding = if needs_commit {
            let mut binding =
                Binding::new(session.clone(), account.id, &waiter.model, generation + 1);
            binding.group_id = waiter.group_id;
            binding
        } else {
            previous.ok_or_else(Error::storage)?
        };
        let remove = s.queues.get_mut(&session).is_some_and(|q| {
            q.remove(position);
            q.is_empty()
        });
        s.order.retain(|key| key != &session);
        if remove {
            s.queues.remove(&session);
        } else {
            s.order.push_back(session.clone());
        }
        *s.inflight.entry(account.id).or_default() += 1;
        let active = s.active_sessions.entry(session.clone()).or_default();
        active.threads.insert(waiter.thread_id.clone(), id);
        if needs_commit {
            active.binding_pending = Some(id);
        }
        s.leases.insert(id, 0);
        drop(s);
        self.wake();
        Ok(Some(DispatchLease {
            source: waiter.source,
            scheduler: self.clone(),
            id,
            session,
            thread_id: waiter.thread_id,
            account,
            binding,
            needs_commit,
            expected_generation: generation,
            model: waiter.model,
            group_id: waiter.group_id,
        }))
    }
    fn remove_waiter(&self, id: Uuid, session: &SessionKey) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = s.queues.get_mut(session) {
            queue.retain(|w| w.id != id);
        }
        if s.queues.get(session).is_some_and(VecDeque::is_empty) {
            s.queues.remove(session);
            s.order.retain(|v| v != session);
        }
        drop(s);
        self.wake();
    }
    pub fn stats(&self) -> QueueStats {
        let cfg = self.0.settings.current();
        let s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let requests = s
            .order
            .iter()
            .flat_map(|key| {
                s.queues
                    .get(key)
                    .into_iter()
                    .flatten()
                    .map(|w| QueuedRequest {
                        request_id: w.id,
                        session_id: key.client_session_id.clone(),
                        thread_id: w.thread_id.clone(),
                        session_inflight: s.active_sessions.get(key).map_or(0, |a| a.threads.len()),
                        session_max_inflight: cfg.session_max_inflight,
                        account_id: s.bindings.get(key).map(|b| b.account_id),
                        wait_ms: w.since.elapsed().as_millis() as u64,
                    })
            })
            .collect::<Vec<_>>();
        let accounts = s
            .accounts
            .values()
            .map(|a| AccountRuntime {
                id: a.id,
                inflight: s.inflight.get(&a.id).copied().unwrap_or(0),
                queued: requests
                    .iter()
                    .filter(|r| r.account_id == Some(a.id))
                    .count(),
                max_inflight: a.max_inflight,
            })
            .collect();
        QueueStats {
            queued: requests.len(),
            inflight: s.inflight.values().sum(),
            paused: s.paused,
            accounts,
            requests,
        }
    }
}

pub struct QueueTicket {
    scheduler: Scheduler,
    id: Uuid,
    session: SessionKey,
    since: Instant,
}
impl QueueTicket {
    pub async fn wait(self, cancel: &CancellationToken) -> Result<DispatchLease> {
        let mut changes = self.scheduler.0.settings.subscribe();
        loop {
            let notified = self.scheduler.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let deadline =
                self.since + Duration::from_millis(changes.borrow_and_update().queue_timeout_ms);
            if Instant::now() >= deadline {
                return Err(Error::new(
                    429,
                    "queue_timeout",
                    "The configured queue waiting time was exceeded",
                ));
            }
            if cancel.is_cancelled() {
                return Err(Error::cancelled());
            }
            if let Some(lease) = self.scheduler.try_reserve(self.id)? {
                return Ok(lease);
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::cancelled()),
                _ = changes.changed() => {},
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(deadline) => {},
            }
        }
    }
}
impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.scheduler.remove_waiter(self.id, &self.session);
    }
}

pub struct DispatchLease {
    source: ClientSource,
    model: ModelRef,
    group_id: Uuid,
    scheduler: Scheduler,
    id: Uuid,
    session: SessionKey,
    thread_id: String,
    pub account: Account,
    pub binding: Binding,
    pub needs_commit: bool,
    pub expected_generation: i64,
}
impl DispatchLease {
    /// Authorize one send without retaining a client-session binding.
    pub fn confirm_stateless(&mut self) -> Result<()> {
        if !self.needs_commit || self.expected_generation != 0 {
            return Err(Error::storage());
        }
        let mut state = self
            .scheduler
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(active) = state.active_sessions.get_mut(&self.session)
            && active.binding_pending == Some(self.id)
        {
            active.binding_pending = None;
        }
        self.needs_commit = false;
        drop(state);
        self.scheduler.wake();
        Ok(())
    }
    pub fn confirm_binding(&mut self) {
        let mut state = self
            .scheduler
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state
            .bindings
            .insert(self.session.clone(), self.binding.clone());
        if let Some(active) = state.active_sessions.get_mut(&self.session)
            && active.binding_pending == Some(self.id)
        {
            active.binding_pending = None;
        }
        self.needs_commit = false;
        drop(state);
        self.scheduler.wake();
    }
    pub fn begin_send(&self) -> Result<()> {
        self.begin_attempt(0)
    }
    /// One additional send, only after the provider confirms a recoverable
    /// encrypted-reasoning HTTP rejection. Retains the original capacity lease.
    pub(crate) fn begin_encrypted_reasoning_recovery(&self) -> Result<()> {
        self.begin_attempt(1)
    }
    fn begin_attempt(&self, previous_attempts: u8) -> Result<()> {
        let mut s = self
            .scheduler
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if s.paused {
            return Err(Error::storage());
        }
        s.check_key(self.session.key_id, self.group_id)?;
        if s.blocked.contains(&self.account.id)
            || !s.accounts.get(&self.account.id).is_some_and(|a| {
                a.enabled
                    && a.group_ids.contains(&self.group_id)
                    && a.supports(&self.model)
                    && a.accepts(self.source)
            })
        {
            return Err(Error::new(
                409,
                "reservation_invalidated",
                "The account state or group changed before dispatch",
            ));
        }
        if self.needs_commit {
            return Err(Error::storage());
        }
        if s.leases.get(&self.id) != Some(&previous_attempts) {
            return Err(Error::new(
                500,
                "attempt_already_consumed",
                "The upstream attempt has already been consumed",
            ));
        }
        s.leases.insert(self.id, previous_attempts + 1);
        *s.served.entry(self.account.id).or_default() += 1;
        Ok(())
    }
}
impl Drop for DispatchLease {
    fn drop(&mut self) {
        let mut s = self
            .scheduler
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if s.leases.remove(&self.id).is_some() {
            if let Some(count) = s.inflight.get_mut(&self.account.id) {
                *count = count.saturating_sub(1);
            }
            if let Some(active) = s.active_sessions.get_mut(&self.session) {
                active.threads.remove(&self.thread_id);
                if active.binding_pending == Some(self.id) {
                    active.binding_pending = None;
                }
                if active.threads.is_empty() {
                    s.active_sessions.remove(&self.session);
                }
            }
        }
        drop(s);
        self.scheduler.wake();
    }
}

#[derive(Clone)]
pub struct MemoryBudget {
    used: Arc<Mutex<usize>>,
    settings: LiveSettings,
}
impl MemoryBudget {
    pub fn new(settings: LiveSettings) -> Self {
        Self {
            used: Arc::new(Mutex::new(0)),
            settings,
        }
    }
    pub fn used(&self) -> usize {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn lease(&self) -> MemoryLease {
        MemoryLease {
            budget: self.clone(),
            bytes: 0,
        }
    }
}
pub struct MemoryLease {
    budget: MemoryBudget,
    bytes: usize,
}
impl MemoryLease {
    pub fn resize(&mut self, bytes: usize) -> Result<()> {
        if bytes >= self.bytes {
            self.grow(bytes - self.bytes)
        } else {
            let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
            *used -= self.bytes - bytes;
            self.bytes = bytes;
            Ok(())
        }
    }
    pub fn grow(&mut self, bytes: usize) -> Result<()> {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        let next = used
            .checked_add(bytes)
            .ok_or_else(|| Error::new(429, "memory_limit", "Request memory budget is exhausted"))?;
        if next > self.budget.settings.current().queue_memory_bytes {
            return Err(Error::new(
                429,
                "memory_limit",
                "Request memory budget is exhausted",
            ));
        }
        *used = next;
        self.bytes += bytes;
        Ok(())
    }
}
pub struct BudgetedBytes {
    pub bytes: bytes::Bytes,
    pub lease: MemoryLease,
}
impl AsRef<[u8]> for BudgetedBytes {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}
impl Drop for MemoryLease {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(self.bytes);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{accounts::ClientProfile, groups::DEFAULT_GROUP_ID, settings::RuntimeSettings};
    fn account() -> Account {
        Account {
            id: Uuid::new_v4(),
            group_ids: crate::groups::default_group_ids(),
            name: "test".into(),
            provider: "openai".into(),
            access_kind: "codex_oauth".into(),
            enabled: true,
            codex_only: false,
            disable_reason: None,
            max_inflight: 1,
            upstream_account_id: "upstream".into(),
            upstream_base_url: "https://example.test".into(),
            models: vec![],
            models_restricted: false,
            model_catalog: None,
            profile: ClientProfile::default(),
            version: 1,
            credential_version: 1,
            credential_expires_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn key(group_id: Uuid) -> GatewayKey {
        GatewayKey {
            id: Uuid::new_v4(),
            group_id,
            name: "test".into(),
            prefix: "sk-test".into(),
            enabled: true,
            created_at: chrono::Utc::now(),
            last_used_at: None,
        }
    }
    fn session(key: &GatewayKey, name: &str) -> SessionKey {
        SessionKey {
            key_id: key.id,
            client_session_id: name.into(),
        }
    }
    fn enqueue(s: &Scheduler, key: &GatewayKey, name: &str) -> Result<QueueTicket> {
        enqueue_thread(s, key, name, name)
    }
    fn enqueue_thread(
        s: &Scheduler,
        key: &GatewayKey,
        session_name: &str,
        thread_name: &str,
    ) -> Result<QueueTicket> {
        s.enqueue(
            Uuid::new_v4(),
            ThreadKey {
                session: session(key, session_name),
                client_thread_id: thread_name.into(),
            },
            key.group_id,
            ModelRef::codex("test"),
            ClientSource::Unknown,
            Instant::now(),
        )
    }
    #[tokio::test(start_paused = true)]
    async fn session_cap_shares_one_binding_and_leaves_capacity_for_other_sessions() {
        let mut a = account();
        a.max_inflight = 10;
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a],
            vec![],
            vec![k.clone()],
        );
        let first_ticket = enqueue_thread(&s, &k, "session", "a").unwrap();
        let mut first = s.try_reserve(first_ticket.id).unwrap().unwrap();
        let second_ticket = enqueue_thread(&s, &k, "session", "b").unwrap();
        let third_ticket = enqueue_thread(&s, &k, "session", "c").unwrap();
        // No second binding/namespace may be created while the first commit is pending.
        assert!(s.try_reserve(second_ticket.id).unwrap().is_none());
        first.confirm_binding();
        first.begin_send().unwrap();
        let second = s.try_reserve(second_ticket.id).unwrap().unwrap();
        assert!(!second.needs_commit);
        assert_eq!(second.binding.id, first.binding.id);
        assert_eq!(second.binding.namespace, first.binding.namespace);
        second.begin_send().unwrap();
        assert!(s.try_reserve(third_ticket.id).unwrap().is_none());
        let queued = s.stats().requests;
        assert_eq!(queued[0].thread_id, "c");
        assert_eq!(queued[0].session_inflight, 2);
        assert_eq!(queued[0].session_max_inflight, 2);

        let other_ticket = enqueue_thread(&s, &k, "other-session", "a").unwrap();
        let mut other = s.try_reserve(other_ticket.id).unwrap().unwrap();
        other.confirm_binding();
        other.begin_send().unwrap();
        assert_eq!(s.stats().inflight, 3);
        drop(first);
        let third = s.try_reserve(third_ticket.id).unwrap().unwrap();
        assert_eq!(third.binding.id, second.binding.id);
        third.begin_send().unwrap();
        drop((second, third, other));
        assert_eq!(s.stats().inflight, 0);
        assert!(s.0.state.lock().unwrap().active_sessions.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn thread_fifo_does_not_block_an_independent_thread_in_the_same_session() {
        let mut a = account();
        a.max_inflight = 10;
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a],
            vec![],
            vec![k.clone()],
        );
        let first_ticket = enqueue_thread(&s, &k, "session", "a").unwrap();
        let mut first = s.try_reserve(first_ticket.id).unwrap().unwrap();
        first.confirm_binding();
        first.begin_send().unwrap();
        let next_a = enqueue_thread(&s, &k, "session", "a").unwrap();
        let last_a = enqueue_thread(&s, &k, "session", "a").unwrap();
        let next_b = enqueue_thread(&s, &k, "session", "b").unwrap();
        assert!(s.try_reserve(next_a.id).unwrap().is_none());
        let parallel = s.try_reserve(next_b.id).unwrap().unwrap();
        parallel.begin_send().unwrap();
        drop(first);
        assert!(s.try_reserve(last_a.id).unwrap().is_none());
        let next = s.try_reserve(next_a.id).unwrap().unwrap();
        next.begin_send().unwrap();
        assert!(s.try_reserve(last_a.id).unwrap().is_none());
        drop(next);
        let last = s.try_reserve(last_a.id).unwrap().unwrap();
        last.begin_send().unwrap();
        assert_eq!(s.stats().inflight, 2);
        drop((last, parallel));
    }

    #[tokio::test(start_paused = true)]
    async fn session_cap_hot_updates_wake_waiters_and_never_cancel_active_threads() {
        let mut a = account();
        a.max_inflight = 10;
        let k = key(DEFAULT_GROUP_ID);
        let live = LiveSettings::new(RuntimeSettings::default());
        let s = Scheduler::new(live.clone(), vec![a], vec![], vec![k.clone()]);
        let first_ticket = enqueue_thread(&s, &k, "session", "a").unwrap();
        let mut first = s.try_reserve(first_ticket.id).unwrap().unwrap();
        first.confirm_binding();
        first.begin_send().unwrap();
        let second_ticket = enqueue_thread(&s, &k, "session", "b").unwrap();
        let second = s.try_reserve(second_ticket.id).unwrap().unwrap();
        second.begin_send().unwrap();
        let ticket = enqueue_thread(&s, &k, "session", "c").unwrap();
        let task = tokio::spawn(async move { ticket.wait(&CancellationToken::new()).await });
        tokio::task::yield_now().await;
        let mut cfg = live.current();
        cfg.session_max_inflight = 1;
        live.publish(cfg.clone());
        assert_eq!(s.stats().inflight, 2);
        drop(first);
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        cfg.session_max_inflight = 2;
        live.publish(cfg);
        let third = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        third.begin_send().unwrap();
        assert_eq!(s.stats().inflight, 2);
        let cancelled = CancellationToken::new();
        let ticket = enqueue_thread(&s, &k, "session", "d").unwrap();
        cancelled.cancel();
        assert_eq!(
            ticket.wait(&cancelled).await.err().unwrap().code,
            "client_cancelled"
        );
        assert_eq!(s.stats().queued, 0);
        drop((second, third));
    }

    #[tokio::test(start_paused = true)]
    async fn parallel_session_migration_and_group_change_wait_for_every_old_lease() {
        let mut a = account();
        a.max_inflight = 10;
        let mut b = account();
        b.max_inflight = 10;
        let mut k = key(DEFAULT_GROUP_ID);
        let binding = Binding::new(session(&k, "s"), a.id, &ModelRef::codex("test"), 1);
        let live = LiveSettings::new(RuntimeSettings {
            session_max_inflight: 3,
            ..Default::default()
        });
        let s = Scheduler::new(
            live,
            vec![a.clone(), b.clone()],
            vec![binding],
            vec![k.clone()],
        );
        let ta = enqueue_thread(&s, &k, "s", "a").unwrap();
        let first = s.try_reserve(ta.id).unwrap().unwrap();
        first.begin_send().unwrap();
        let tb = enqueue_thread(&s, &k, "s", "b").unwrap();
        let second = s.try_reserve(tb.id).unwrap().unwrap();
        second.begin_send().unwrap();
        a.enabled = false;
        s.update_account(a);
        let tc = enqueue_thread(&s, &k, "s", "c").unwrap();
        assert!(s.try_reserve(tc.id).unwrap().is_none());
        drop(first);
        assert!(s.try_reserve(tc.id).unwrap().is_none());
        drop(second);
        let mut third = s.try_reserve(tc.id).unwrap().unwrap();
        assert_eq!(third.account.id, b.id);
        assert_eq!(third.binding.generation, 2);
        let td = enqueue_thread(&s, &k, "s", "d").unwrap();
        assert!(s.try_reserve(td.id).unwrap().is_none());
        third.confirm_binding();
        third.begin_send().unwrap();
        let fourth = s.try_reserve(td.id).unwrap().unwrap();
        assert_eq!(fourth.binding.id, third.binding.id);
        fourth.begin_send().unwrap();

        k.group_id = Uuid::new_v4();
        b.group_ids.push(k.group_id);
        s.update_account(b);
        s.update_key(k.clone());
        let te = enqueue_thread(&s, &k, "s", "e").unwrap();
        assert!(s.try_reserve(te.id).unwrap().is_none());
        drop(third);
        assert!(s.try_reserve(te.id).unwrap().is_none());
        drop(fourth);
        let next = s.try_reserve(te.id).unwrap().unwrap();
        assert_eq!(next.binding.generation, 3);
        assert_eq!(next.binding.group_id, k.group_id);
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_binding_reservation_releases_the_whole_session() {
        let mut a = account();
        a.max_inflight = 10;
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a],
            vec![],
            vec![k.clone()],
        );
        let ta = enqueue_thread(&s, &k, "s", "a").unwrap();
        let first = s.try_reserve(ta.id).unwrap().unwrap();
        let tb = enqueue_thread(&s, &k, "s", "b").unwrap();
        assert!(s.try_reserve(tb.id).unwrap().is_none());
        let abandoned = first.binding.id;
        drop(first);
        let mut next = s.try_reserve(tb.id).unwrap().unwrap();
        assert_ne!(next.binding.id, abandoned);
        assert_eq!(next.expected_generation, 0);
        assert_eq!(next.binding.generation, 1);
        next.confirm_binding();
        next.begin_send().unwrap();
        drop(next);
        assert_eq!(s.stats().inflight, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stateless_leases_do_not_serialize_by_key_or_retain_bindings() {
        let mut a = account();
        a.max_inflight = 2;
        let k = key(a.group_ids[0]);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a],
            vec![],
            vec![k.clone()],
        );
        let cancel = CancellationToken::new();
        let mut first = enqueue(&s, &k, "request-one")
            .unwrap()
            .wait(&cancel)
            .await
            .unwrap();
        first.confirm_stateless().unwrap();
        first.begin_send().unwrap();
        let mut second = enqueue(&s, &k, "request-two")
            .unwrap()
            .wait(&cancel)
            .await
            .unwrap();
        second.confirm_stateless().unwrap();
        second.begin_send().unwrap();
        assert_eq!(s.stats().inflight, 2);
        assert!(s.0.state.lock().unwrap().bindings.is_empty());
        assert_eq!(
            first.begin_send().unwrap_err().code,
            "attempt_already_consumed"
        );
        drop(first);
        drop(second);
        assert_eq!(s.stats().inflight, 0);
        let state = s.0.state.lock().unwrap();
        assert!(
            state.bindings.is_empty()
                && state.queues.is_empty()
                && state.active_sessions.is_empty()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn source_policy_is_checked_before_queueing_and_sending() {
        let mut a = account();
        let k = key(a.group_ids[0]);
        let scheduler = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        let cancel = CancellationToken::new();
        let mut reserved = enqueue(&scheduler, &k, "reserved")
            .unwrap()
            .wait(&cancel)
            .await
            .unwrap();
        reserved.confirm_binding();
        a.codex_only = true;
        scheduler.update_account(a.clone());
        assert_eq!(
            reserved.begin_send().unwrap_err().code,
            "reservation_invalidated"
        );
        drop(reserved);
        assert_eq!(
            enqueue(&scheduler, &k, "unknown").err().unwrap().code,
            "client_source_not_allowed"
        );
        assert_eq!(scheduler.stats().queued, 0);
        let ticket = scheduler
            .enqueue(
                Uuid::new_v4(),
                ThreadKey {
                    session: session(&k, "codex"),
                    client_thread_id: "codex".into(),
                },
                k.group_id,
                ModelRef::codex("test"),
                ClientSource::Codex,
                Instant::now(),
            )
            .unwrap();
        let mut allowed = ticket.wait(&cancel).await.unwrap();
        allowed.confirm_binding();
        allowed.begin_send().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn source_policy_change_rejects_queued_requests_without_waiting_for_capacity() {
        let mut a = account();
        a.max_inflight = 1;
        let k = key(a.group_ids[0]);
        let scheduler = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        let cancel = CancellationToken::new();
        let mut active = enqueue(&scheduler, &k, "active")
            .unwrap()
            .wait(&cancel)
            .await
            .unwrap();
        active.confirm_binding();
        active.begin_send().unwrap();
        let queued = enqueue(&scheduler, &k, "waiting").unwrap();
        a.codex_only = true;
        scheduler.update_account(a);
        assert_eq!(
            queued.wait(&cancel).await.err().unwrap().code,
            "client_source_not_allowed"
        );
        assert_eq!(scheduler.stats().inflight, 1);
        assert_eq!(scheduler.stats().queued, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn source_policy_migrates_a_bound_session_only_to_an_allowed_account() {
        let mut a = account();
        let mut b = account();
        b.enabled = false;
        let k = key(a.group_ids[0]);
        let scheduler = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone(), b.clone()],
            vec![],
            vec![k.clone()],
        );
        let cancel = CancellationToken::new();
        let mut first = enqueue(&scheduler, &k, "s")
            .unwrap()
            .wait(&cancel)
            .await
            .unwrap();
        first.confirm_binding();
        first.begin_send().unwrap();
        let namespace = first.binding.namespace;
        drop(first);
        a.codex_only = true;
        b.enabled = true;
        scheduler.update_account(a);
        scheduler.update_account(b.clone());
        let mut second = enqueue(&scheduler, &k, "s")
            .unwrap()
            .wait(&cancel)
            .await
            .unwrap();
        assert_eq!(second.account.id, b.id);
        assert_eq!(second.binding.generation, 2);
        assert_ne!(second.binding.namespace, namespace);
        second.confirm_binding();
        second.begin_send().unwrap();
    }
    #[test]
    fn no_matching_group_account_is_rejected_without_queueing() {
        let g = Uuid::new_v4();
        let k = key(g);
        let mut a = account();
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        assert_eq!(
            enqueue(&s, &k, "new").err().unwrap().code,
            "no_available_account"
        );
        a.group_ids = vec![g];
        a.enabled = false;
        s.update_account(a.clone());
        assert_eq!(
            enqueue(&s, &k, "disabled").err().unwrap().code,
            "no_available_account"
        );
        a.enabled = true;
        a.models_restricted = true;
        a.models = vec!["other".into()];
        s.update_account(a.clone());
        assert_eq!(
            enqueue(&s, &k, "unsupported").err().unwrap().code,
            "no_available_account"
        );
        a.models.clear();
        s.update_account(a);
        assert_eq!(
            enqueue(&s, &k, "empty-models").err().unwrap().code,
            "no_available_account"
        );
        assert_eq!(s.stats().queued, 0);
    }
    #[tokio::test(start_paused = true)]
    async fn saturation_waits_and_disable_migrates_after_drain_within_group() {
        let a = account();
        let b = account();
        let k = key(DEFAULT_GROUP_ID);
        let binding = Binding::new(session(&k, "main"), a.id, &ModelRef::codex("test"), 1);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone(), b.clone()],
            vec![binding],
            vec![k.clone()],
        );
        let first = enqueue(&s, &k, "main")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        let ticket = enqueue(&s, &k, "main").unwrap();
        assert!(s.try_reserve(ticket.id).unwrap().is_none());
        s.block_account(a.id);
        assert!(s.try_reserve(ticket.id).unwrap().is_none());
        let mut disabled = a.clone();
        disabled.enabled = false;
        s.update_account(disabled);
        assert!(s.try_reserve(ticket.id).unwrap().is_none());
        drop(first);
        let next = ticket.wait(&CancellationToken::new()).await.unwrap();
        assert_eq!(next.account.id, b.id);
        assert_eq!(next.binding.generation, 2);
        assert_eq!(next.binding.group_id, DEFAULT_GROUP_ID);
    }
    #[tokio::test(start_paused = true)]
    async fn full_capacity_waiters_fail_when_the_last_group_account_disappears() {
        let mut a = account();
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        let first = enqueue(&s, &k, "first")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        let ticket = enqueue(&s, &k, "queued").unwrap();
        let task =
            tokio::spawn(
                async move { ticket.wait(&CancellationToken::new()).await.err().unwrap() },
            );
        tokio::task::yield_now().await;
        a.group_ids.clear();
        s.update_account(a);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .code,
            "no_available_account"
        );
        assert_eq!(s.stats().queued, 0);
        drop(first);
    }
    #[tokio::test(start_paused = true)]
    async fn key_scope_changes_cancel_waiters_even_behind_global_capacity() {
        let a = account();
        let k = key(DEFAULT_GROUP_ID);
        let mut other = key(DEFAULT_GROUP_ID);
        let cfg = RuntimeSettings {
            global_max_inflight: 1,
            ..Default::default()
        };
        let s = Scheduler::new(
            LiveSettings::new(cfg),
            vec![a],
            vec![],
            vec![k.clone(), other.clone()],
        );
        let first = enqueue(&s, &k, "first")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        let ticket = enqueue(&s, &other, "queued").unwrap();
        other.group_id = Uuid::new_v4();
        s.update_key(other);
        assert_eq!(
            ticket
                .wait(&CancellationToken::new())
                .await
                .err()
                .unwrap()
                .code,
            "key_group_changed"
        );
        drop(first);
        assert_eq!(s.stats().queued, 0);
    }
    #[tokio::test(start_paused = true)]
    async fn hot_timeout_applies_only_to_capacity_waiters() {
        let live = LiveSettings::new(RuntimeSettings::default());
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(live.clone(), vec![account()], vec![], vec![k.clone()]);
        let first = enqueue(&s, &k, "first")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        let ticket = enqueue(&s, &k, "queued").unwrap();
        let task =
            tokio::spawn(
                async move { ticket.wait(&CancellationToken::new()).await.err().unwrap() },
            );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut cfg = live.current();
        cfg.queue_timeout_ms = 1000;
        live.publish(cfg);
        assert_eq!(task.await.unwrap().code, "queue_timeout");
        drop(first);
        assert_eq!(s.stats().queued, 0);
    }
    #[tokio::test]
    async fn group_change_renews_binding_even_on_a_shared_account() {
        let mut a = account();
        let g = Uuid::new_v4();
        a.group_ids.push(g);
        let mut k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        let mut first = enqueue(&s, &k, "main")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        first.confirm_binding();
        let old = first.binding.namespace;
        drop(first);
        k.group_id = g;
        s.update_key(k.clone());
        let second = enqueue(&s, &k, "main")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(second.account.id, a.id);
        assert_eq!(second.binding.group_id, g);
        assert_eq!(second.binding.generation, 2);
        assert_ne!(second.binding.namespace, old);
    }
    #[tokio::test]
    async fn dispatch_rechecks_membership_and_key_scope_after_reserving() {
        let mut a = account();
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        let mut lease = enqueue(&s, &k, "main")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        lease.confirm_binding();
        a.group_ids.clear();
        s.update_account(a.clone());
        assert_eq!(
            lease.begin_send().err().unwrap().code,
            "reservation_invalidated"
        );
        a.group_ids = vec![DEFAULT_GROUP_ID];
        s.update_account(a);
        let mut disabled = k;
        disabled.enabled = false;
        s.update_key(disabled);
        assert_eq!(lease.begin_send().err().unwrap().code, "invalid_api_key");
        drop(lease);
        assert_eq!(s.stats().inflight, 0);
    }
    #[tokio::test]
    async fn permit_is_single_use_and_cancelled_tickets_disappear() {
        let k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![account()],
            vec![],
            vec![k.clone()],
        );
        let mut lease = enqueue(&s, &k, "first")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        lease.confirm_binding();
        assert!(lease.begin_send().is_ok());
        assert!(lease.begin_send().is_err());
        let ticket = enqueue(&s, &k, "cancel").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(ticket.wait(&cancel).await.is_err());
        assert_eq!(s.stats().queued, 0);
    }

    #[tokio::test]
    async fn recovery_permit_is_bounded_and_rechecks_account_and_key() {
        let mut a = account();
        let mut k = key(DEFAULT_GROUP_ID);
        let s = Scheduler::new(
            LiveSettings::new(RuntimeSettings::default()),
            vec![a.clone()],
            vec![],
            vec![k.clone()],
        );
        let mut lease = enqueue(&s, &k, "recovery")
            .unwrap()
            .wait(&CancellationToken::new())
            .await
            .unwrap();
        lease.confirm_binding();
        assert!(lease.begin_encrypted_reasoning_recovery().is_err());
        lease.begin_send().unwrap();
        a.enabled = false;
        s.update_account(a.clone());
        assert_eq!(
            lease.begin_encrypted_reasoning_recovery().unwrap_err().code,
            "reservation_invalidated"
        );
        a.enabled = true;
        a.group_ids.clear();
        s.update_account(a.clone());
        assert_eq!(
            lease.begin_encrypted_reasoning_recovery().unwrap_err().code,
            "reservation_invalidated"
        );
        a.group_ids = vec![DEFAULT_GROUP_ID];
        s.update_account(a);
        k.enabled = false;
        s.update_key(k.clone());
        assert_eq!(
            lease.begin_encrypted_reasoning_recovery().unwrap_err().code,
            "invalid_api_key"
        );
        k.enabled = true;
        s.update_key(k);
        lease.begin_encrypted_reasoning_recovery().unwrap();
        assert_eq!(s.stats().inflight, 1);
        assert!(lease.begin_send().is_err());
        assert!(lease.begin_encrypted_reasoning_recovery().is_err());
        drop(lease);
        assert_eq!(s.stats().inflight, 0);
    }
}
