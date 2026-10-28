use crate::{Error, Result, identity::SessionKey};
use std::{collections::HashMap, sync::Mutex};

/// Only explicit upstream safety decisions, keyed by authenticated key + session.
/// No request body, model, thread or account participates in this decision.
#[derive(Default)]
pub(crate) struct SafetyGuard(Mutex<HashMap<SessionKey, (u32, Error)>>);
impl SafetyGuard {
    pub fn check(&self, session: &SessionKey) -> Result<()> {
        match self.0.lock().unwrap().get(session) {
            Some((count, previous)) if *count >= 3 => Err(blocked(previous.clone())),
            _ => Ok(()),
        }
    }
    pub fn restore(&self, session: SessionKey, count: u32, error: Error) {
        let mut sessions = self.0.lock().unwrap();
        let entry = sessions.entry(session).or_insert((count, error.clone()));
        if count > entry.0 {
            *entry = (count, error);
        }
    }
    pub fn remember(&self, session: SessionKey, error: Error) {
        let saved = Error::new(error.status, &error.code, &error.message)
            .with_upstream(error.upstream.map(|facts| *facts));
        let mut sessions = self.0.lock().unwrap();
        let entry = sessions.entry(session).or_insert((0, saved));
        entry.0 = (entry.0 + 1).min(3);
    }
}

pub(crate) fn blocked(previous: Error) -> Error {
    Error::new(403, "session_safety_blocked", format!(
        "The gateway blocked this session without contacting the upstream because at least 3 requests in this session were rejected by upstream safety policy. Previous rejection: {}", previous.message
    )).with_upstream(previous.upstream.map(|facts| *facts))
}
