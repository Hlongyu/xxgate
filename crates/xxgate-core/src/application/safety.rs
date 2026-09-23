use crate::{Error, Result, identity::SessionKey};
use std::{collections::HashMap, sync::Mutex};

/// Only explicit upstream safety decisions, keyed by authenticated key + session.
/// No request body, model, thread or account participates in this decision.
#[derive(Default)]
pub(crate) struct SafetyGuard(Mutex<HashMap<SessionKey, Error>>);
impl SafetyGuard {
    pub fn check(&self, session: &SessionKey) -> Result<()> {
        match self.0.lock().unwrap().get(session) {
            Some(previous) => Err(blocked(previous.clone())),
            None => Ok(()),
        }
    }
    pub fn remember(&self, session: SessionKey, error: Error) {
        let saved = Error::new(error.status, &error.code, &error.message)
            .with_upstream(error.upstream.map(|facts| *facts));
        self.0.lock().unwrap().entry(session).or_insert(saved);
    }
}

pub(crate) fn blocked(previous: Error) -> Error {
    Error::new(403, "session_safety_blocked", format!(
        "The gateway blocked this session without contacting the upstream because an earlier request in this session was rejected by upstream safety policy. Previous rejection: {}", previous.message
    )).with_upstream(previous.upstream.map(|facts| *facts))
}
