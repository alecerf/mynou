//! In-memory browser sessions. No API token is stored in a session or cookie.
use crate::{
    Result,
    crypto::{constant_time_eq, random_bytes},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub const SESSION_SECS: u64 = 28_800;
const CHALLENGE_SECS: u64 = 600;
const MAX_SESSIONS: usize = 128;

#[derive(Clone)]
struct SharedPreview {
    series_id: String,
    query: crate::pack::SharedFileRequest,
    expires: Instant,
}
#[derive(Clone)]
struct GroupPreview {
    owner_id: String,
    query: crate::library::GroupRequest,
    expires: Instant,
}

#[derive(Clone)]
struct RequesterPreview {
    account_id: String,
    query: crate::requesters::ControlRequest,
    expires: Instant,
}

#[derive(Clone)]
struct IrcPreview {
    announcement_id: String,
    query: crate::irc::ControlRequest,
    expires: Instant,
}

#[derive(Clone)]
struct NotificationPreview {
    query: crate::notifications::ControlRequest,
    expires: Instant,
}

#[derive(Clone)]
struct IndexerPreview {
    source_id: String,
    query: crate::indexers::ControlRequest,
    expires: Instant,
}

#[derive(Clone)]
struct UsenetPreview {
    server_id: String,
    query: crate::usenet::ProbeRequest,
    expires: Instant,
}

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub csrf: String,
    pub authority: String,
    pub origin: Option<String>,
    pub secure: bool,
    pub messages: Vec<String>,
    expires: Instant,
    attempts: u8,
    shared_preview: Option<SharedPreview>,
    group_preview: Option<GroupPreview>,
    requester_preview: Option<RequesterPreview>,
    irc_preview: Option<IrcPreview>,
    notification_preview: Option<NotificationPreview>,
    indexer_preview: Option<IndexerPreview>,
    usenet_preview: Option<UsenetPreview>,
}

pub struct Sessions(BTreeMap<String, Session>);

impl Sessions {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.0.retain(|_, session| session.expires > now);
    }

    pub fn get(&mut self, id: &str, authority: &str) -> Option<Session> {
        self.purge();
        self.0
            .get(id)
            .filter(|session| session.authority == authority)
            .cloned()
    }

    pub fn challenge(&mut self, authority: &str) -> Result<Session> {
        self.purge();
        if self.0.len() >= MAX_SESSIONS {
            let oldest = self
                .0
                .values()
                .filter(|session| session.origin.is_none())
                .min_by_key(|session| session.expires)
                .map(|session| session.id.clone());
            if let Some(id) = oldest {
                self.0.remove(&id);
            } else {
                return Err("All browser sessions are in use. Try again later".into());
            }
        }
        let session = Session {
            id: nonce()?,
            csrf: nonce()?,
            authority: authority.to_owned(),
            origin: None,
            secure: false,
            messages: Vec::new(),
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
            attempts: 0,
            shared_preview: None,
            group_preview: None,
            requester_preview: None,
            irc_preview: None,
            notification_preview: None,
            indexer_preview: None,
            usenet_preview: None,
        };
        self.0.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub fn login(
        &mut self,
        id: &str,
        csrf: &str,
        origin: &str,
        valid_token: bool,
    ) -> Result<Option<Session>> {
        self.purge();
        let current = self
            .0
            .get_mut(id)
            .ok_or("The sign-in form expired. Reload it and try again")?;
        if current.origin.is_some() || !constant_time_eq(current.csrf.as_bytes(), csrf.as_bytes()) {
            return Err("The sign-in form is invalid. Reload it and try again".into());
        }
        current.attempts += 1;
        if !valid_token {
            if current.attempts >= 5 {
                self.0.remove(id);
            }
            return Ok(None);
        }
        // Obtain all randomness before consuming the challenge or changing identity.
        let new_id = nonce()?;
        let new_csrf = nonce()?;
        let mut session = self.0.remove(id).ok_or("The sign-in form expired")?;
        session.id = new_id;
        session.csrf = new_csrf;
        session.secure = origin.starts_with("https://");
        session.origin = Some(origin.to_owned());
        session.expires = Instant::now() + Duration::from_secs(SESSION_SECS);
        session.attempts = 0;
        self.0.insert(session.id.clone(), session.clone());
        Ok(Some(session))
    }

    pub fn remove(&mut self, id: &str) {
        self.0.remove(id);
    }

    pub fn message(&mut self, id: &str, messages: Vec<String>) {
        if let Some(session) = self.0.get_mut(id) {
            session.messages = messages
                .into_iter()
                .take(34)
                .map(|text| crate::integrations::report_text(&text, 384))
                .collect();
        }
    }

    pub fn take_messages(&mut self, id: &str) -> Vec<String> {
        self.0
            .get_mut(id)
            .map(|session| std::mem::take(&mut session.messages))
            .unwrap_or_default()
    }

    pub fn save_usenet_preview(
        &mut self,
        id: &str,
        server_id: &str,
        query: crate::usenet::ProbeRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply || !crate::requesters::valid_id(server_id) {
            return Err("Usenet: browser review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|s| s.origin.is_some())
            .ok_or("Browser session expired")?;
        session.shared_preview = None;
        session.group_preview = None;
        session.requester_preview = None;
        session.irc_preview = None;
        session.notification_preview = None;
        session.indexer_preview = None;
        session.usenet_preview = Some(UsenetPreview {
            server_id: server_id.into(),
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        Ok(())
    }
    pub fn usenet_preview(
        &mut self,
        id: &str,
        server_id: &str,
        plan_id: &str,
    ) -> Result<crate::usenet::ProbeRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|s| s.usenet_preview.as_ref())
            .filter(|p| {
                p.expires > Instant::now()
                    && p.server_id == server_id
                    && p.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|p| p.query.clone())
            .ok_or("Usenet: browser review expired or changed; preview again".into())
    }
    pub fn clear_usenet_preview(&mut self, id: &str) {
        if let Some(s) = self.0.get_mut(id) {
            s.usenet_preview = None;
        }
    }

    pub fn save_indexer_preview(
        &mut self,
        id: &str,
        source_id: &str,
        query: crate::indexers::ControlRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply || !crate::requesters::valid_id(source_id) {
            return Err("Indexer policy: browser review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|s| s.origin.is_some())
            .ok_or("Browser session expired")?;
        session.shared_preview = None;
        session.group_preview = None;
        session.requester_preview = None;
        session.irc_preview = None;
        session.notification_preview = None;
        session.usenet_preview = None;
        session.indexer_preview = Some(IndexerPreview {
            source_id: source_id.into(),
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        Ok(())
    }
    pub fn indexer_preview(
        &mut self,
        id: &str,
        source_id: &str,
        action: &str,
        plan_id: &str,
    ) -> Result<crate::indexers::ControlRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|s| s.indexer_preview.as_ref())
            .filter(|p| {
                p.expires > Instant::now()
                    && p.source_id == source_id
                    && p.query.action == action
                    && p.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|p| p.query.clone())
            .ok_or("Indexer policy: browser review expired or changed; preview again".into())
    }
    pub fn clear_indexer_preview(&mut self, id: &str) {
        if let Some(s) = self.0.get_mut(id) {
            s.indexer_preview = None;
        }
    }

    pub fn save_notification_preview(
        &mut self,
        id: &str,
        query: crate::notifications::ControlRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply {
            return Err("Notifications: browser review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|s| s.origin.is_some())
            .ok_or("Browser session expired")?;
        session.usenet_preview = None;
        session.indexer_preview = None;
        session.shared_preview = None;
        session.group_preview = None;
        session.requester_preview = None;
        session.irc_preview = None;
        session.notification_preview = Some(NotificationPreview {
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        Ok(())
    }
    pub fn notification_preview(
        &mut self,
        id: &str,
        kind: &str,
        event_id: &str,
        action: &str,
        plan_id: &str,
    ) -> Result<crate::notifications::ControlRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|s| s.notification_preview.as_ref())
            .filter(|p| {
                p.expires > Instant::now()
                    && p.query.kind == kind
                    && p.query.event_id == event_id
                    && p.query.action == action
                    && p.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|p| p.query.clone())
            .ok_or("Notifications: browser review expired or changed; preview again".into())
    }
    pub fn clear_notification_preview(&mut self, id: &str) {
        if let Some(s) = self.0.get_mut(id) {
            s.notification_preview = None;
        }
    }

    /// Keep source credentials server-side, with one bounded review per session.
    pub fn save_irc_preview(
        &mut self,
        id: &str,
        announcement_id: &str,
        query: crate::irc::ControlRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply {
            return Err("IRC: browser review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|s| s.origin.is_some())
            .ok_or("Browser session expired")?;
        session.usenet_preview = None;
        session.indexer_preview = None;
        session.notification_preview = None;
        session.irc_preview = Some(IrcPreview {
            announcement_id: announcement_id.into(),
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        session.shared_preview = None;
        session.group_preview = None;
        session.requester_preview = None;
        Ok(())
    }
    pub fn irc_preview(
        &mut self,
        id: &str,
        announcement_id: &str,
        action: &str,
        plan_id: &str,
    ) -> Result<crate::irc::ControlRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|s| s.irc_preview.as_ref())
            .filter(|p| {
                p.expires > Instant::now()
                    && p.announcement_id == announcement_id
                    && p.query.action == action
                    && p.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|p| p.query.clone())
            .ok_or("IRC: browser review expired or changed; preview again".into())
    }
    pub fn clear_irc_preview(&mut self, id: &str) {
        if let Some(s) = self.0.get_mut(id) {
            s.irc_preview = None;
        }
    }

    pub fn save_requester_preview(
        &mut self,
        id: &str,
        account_id: &str,
        query: crate::requesters::ControlRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply {
            return Err("Requester review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|s| s.origin.is_some())
            .ok_or("Browser session expired")?;
        session.requester_preview = Some(RequesterPreview {
            account_id: account_id.into(),
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        session.usenet_preview = None;
        session.indexer_preview = None;
        session.shared_preview = None;
        session.group_preview = None;
        session.irc_preview = None;
        session.notification_preview = None;
        Ok(())
    }
    pub fn requester_preview(
        &mut self,
        id: &str,
        account_id: &str,
        action: &str,
        plan_id: &str,
    ) -> Result<crate::requesters::ControlRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|s| s.requester_preview.as_ref())
            .filter(|p| {
                p.expires > Instant::now()
                    && p.account_id == account_id
                    && p.query.action == action
                    && p.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|p| p.query.clone())
            .ok_or("Requester browser review expired or changed; preview again".into())
    }
    pub fn clear_requester_preview(&mut self, id: &str) {
        if let Some(s) = self.0.get_mut(id) {
            s.requester_preview = None;
        }
    }

    pub fn save_shared_preview(
        &mut self,
        id: &str,
        series_id: &str,
        query: crate::pack::SharedFileRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply {
            return Err("A shared browser review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|s| s.origin.is_some())
            .ok_or("Browser session expired")?;
        session.shared_preview = Some(SharedPreview {
            series_id: series_id.into(),
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        session.group_preview = None;
        session.requester_preview = None;
        session.irc_preview = None;
        session.notification_preview = None;
        Ok(())
    }
    pub fn shared_preview(
        &mut self,
        id: &str,
        series_id: &str,
        plan_id: &str,
    ) -> Result<crate::pack::SharedFileRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|s| s.shared_preview.as_ref())
            .filter(|p| {
                p.expires > Instant::now()
                    && p.series_id == series_id
                    && p.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|p| p.query.clone())
            .ok_or_else(|| "Shared browser preview expired or changed; preview again".into())
    }
    pub fn clear_shared_preview(&mut self, id: &str) {
        if let Some(session) = self.0.get_mut(id) {
            session.usenet_preview = None;
            session.indexer_preview = None;
            session.shared_preview = None;
        }
    }

    pub fn save_group_preview(
        &mut self,
        id: &str,
        owner_id: &str,
        query: crate::library::GroupRequest,
    ) -> Result<()> {
        query.validate()?;
        if !query.apply {
            return Err("A group browser review requires its apply guard".into());
        }
        self.purge();
        let session = self
            .0
            .get_mut(id)
            .filter(|session| session.origin.is_some())
            .ok_or("Browser session expired")?;
        session.group_preview = Some(GroupPreview {
            owner_id: owner_id.into(),
            query,
            expires: Instant::now() + Duration::from_secs(CHALLENGE_SECS),
        });
        session.usenet_preview = None;
        session.indexer_preview = None;
        session.shared_preview = None;
        session.requester_preview = None;
        session.irc_preview = None;
        session.notification_preview = None;
        Ok(())
    }
    pub fn group_preview(
        &mut self,
        id: &str,
        owner_id: &str,
        plan_id: &str,
    ) -> Result<crate::library::GroupRequest> {
        self.purge();
        self.0
            .get(id)
            .and_then(|session| session.group_preview.as_ref())
            .filter(|preview| {
                preview.expires > Instant::now()
                    && preview.owner_id == owner_id
                    && preview.query.plan_id.as_deref() == Some(plan_id)
            })
            .map(|preview| preview.query.clone())
            .ok_or("Group browser preview expired or changed; preview again".into())
    }
    pub fn clear_group_preview(&mut self, id: &str) {
        if let Some(session) = self.0.get_mut(id) {
            session.group_preview = None;
        }
    }
}

pub fn cookie(session: &Session) -> String {
    let age = if session.origin.is_some() {
        SESSION_SECS
    } else {
        CHALLENGE_SECS
    };
    format!(
        "mynou_session={}; Path=/ui; HttpOnly; SameSite=Strict; Max-Age={age}{}",
        session.id,
        if session.secure { "; Secure" } else { "" }
    )
}

pub fn clear_cookie(secure: bool) -> String {
    format!(
        "mynou_session=; Path=/ui; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

fn nonce() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in random_bytes::<32>()? {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usenet_probe_reviews_bind_session_server_guard_and_expiry() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost").unwrap();
        let session = sessions
            .login(&challenge.id, &challenge.csrf, "http://localhost", true)
            .unwrap()
            .unwrap();
        let q = crate::usenet::ProbeRequest {
            apply: true,
            plan_id: Some("b".repeat(64)),
        };
        sessions
            .save_usenet_preview(&session.id, "provider", q.clone())
            .unwrap();
        assert!(
            sessions
                .usenet_preview(&session.id, "provider", &"b".repeat(64))
                .is_ok()
        );
        assert!(
            sessions
                .usenet_preview("other", "provider", &"b".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .usenet_preview(&session.id, "other", &"b".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .usenet_preview(&session.id, "provider", &"c".repeat(64))
                .is_err()
        );
        sessions
            .0
            .get_mut(&session.id)
            .unwrap()
            .usenet_preview
            .as_mut()
            .unwrap()
            .expires = Instant::now();
        assert!(
            sessions
                .usenet_preview(&session.id, "provider", &"b".repeat(64))
                .is_err()
        );
        sessions
            .save_usenet_preview(&session.id, "provider", q)
            .unwrap();
        sessions.clear_usenet_preview(&session.id);
        assert!(
            sessions
                .usenet_preview(&session.id, "provider", &"b".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn indexer_reviews_bind_source_action_session_guard_and_expiry() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost").unwrap();
        let session = sessions
            .login(&challenge.id, &challenge.csrf, "http://localhost", true)
            .unwrap()
            .unwrap();
        let query = crate::indexers::ControlRequest {
            action: "pause".into(),
            apply: true,
            plan_id: Some("b".repeat(64)),
        };
        sessions
            .save_indexer_preview(&session.id, "primary", query.clone())
            .unwrap();
        assert!(
            sessions
                .indexer_preview(&session.id, "primary", "pause", &"b".repeat(64))
                .is_ok()
        );
        for (id, source, action, guard) in [
            ("other", "primary", "pause", "b".repeat(64)),
            (session.id.as_str(), "other", "pause", "b".repeat(64)),
            (session.id.as_str(), "primary", "probe", "b".repeat(64)),
            (session.id.as_str(), "primary", "pause", "a".repeat(64)),
        ] {
            assert!(
                sessions
                    .indexer_preview(id, source, action, &guard)
                    .is_err()
            );
        }
        sessions
            .0
            .get_mut(&session.id)
            .unwrap()
            .indexer_preview
            .as_mut()
            .unwrap()
            .expires = Instant::now();
        assert!(
            sessions
                .indexer_preview(&session.id, "primary", "pause", &"b".repeat(64))
                .is_err()
        );
        sessions
            .save_indexer_preview(&session.id, "primary", query)
            .unwrap();
        sessions.clear_indexer_preview(&session.id);
        assert!(
            sessions
                .indexer_preview(&session.id, "primary", "pause", &"b".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn irc_reviews_expire_and_bind_record_action_session_and_guard() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost").unwrap();
        let session = sessions
            .login(&challenge.id, &challenge.csrf, "http://localhost", true)
            .unwrap()
            .unwrap();
        let query = crate::irc::ControlRequest {
            action: "dismiss".into(),
            apply: true,
            plan_id: Some("b".repeat(64)),
        };
        sessions
            .save_irc_preview(&session.id, &"a".repeat(64), query.clone())
            .unwrap();
        for (owner, record, action, guard) in [
            (
                session.id.as_str(),
                "c".repeat(64),
                "dismiss",
                "b".repeat(64),
            ),
            (
                session.id.as_str(),
                "a".repeat(64),
                "acknowledge",
                "b".repeat(64),
            ),
            ("other", "a".repeat(64), "dismiss", "b".repeat(64)),
            (
                session.id.as_str(),
                "a".repeat(64),
                "dismiss",
                "c".repeat(64),
            ),
        ] {
            assert!(
                sessions
                    .irc_preview(owner, &record, action, &guard)
                    .is_err()
            );
        }
        assert_eq!(
            sessions
                .irc_preview(&session.id, &"a".repeat(64), "dismiss", &"b".repeat(64))
                .unwrap(),
            query
        );
        sessions
            .0
            .get_mut(&session.id)
            .unwrap()
            .irc_preview
            .as_mut()
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(
            sessions
                .irc_preview(&session.id, &"a".repeat(64), "dismiss", &"b".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn requester_reviews_expire_and_are_bound_to_account_action_and_session() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost").unwrap();
        let session = sessions
            .login(&challenge.id, &challenge.csrf, "http://localhost", true)
            .unwrap()
            .unwrap();
        let query = crate::requesters::ControlRequest {
            action: "remove".into(),
            policy: None,
            demand_id: Some("a".repeat(64)),
            apply: true,
            plan_id: Some("b".repeat(64)),
        };
        sessions
            .save_requester_preview(&session.id, "alice", query.clone())
            .unwrap();
        assert!(
            sessions
                .requester_preview(&session.id, "bob", "remove", &"b".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .requester_preview(&session.id, "alice", "approve", &"b".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .requester_preview("other", "alice", "remove", &"b".repeat(64))
                .is_err()
        );
        assert_eq!(
            sessions
                .requester_preview(&session.id, "alice", "remove", &"b".repeat(64))
                .unwrap(),
            query
        );
        sessions
            .0
            .get_mut(&session.id)
            .unwrap()
            .requester_preview
            .as_mut()
            .unwrap()
            .expires = Instant::now();
        assert!(
            sessions
                .requester_preview(&session.id, "alice", "remove", &"b".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn login_rotates_identifiers_and_expiration_cannot_be_refreshed_by_reads() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost:8080").unwrap();
        assert!(
            sessions
                .login(&challenge.id, "wrong", "http://localhost:8080", true)
                .is_err()
        );
        let session = sessions
            .login(
                &challenge.id,
                &challenge.csrf,
                "http://localhost:8080",
                true,
            )
            .unwrap()
            .unwrap();
        assert_ne!(challenge.id, session.id);
        assert_ne!(challenge.csrf, session.csrf);
        assert!(sessions.get(&challenge.id, "localhost:8080").is_none());
        assert!(sessions.get(&session.id, "other:8080").is_none());
        let expiration = session.expires;
        assert_eq!(
            sessions.get(&session.id, "localhost:8080").unwrap().expires,
            expiration
        );
        sessions.0.get_mut(&session.id).unwrap().expires = Instant::now();
        assert!(sessions.get(&session.id, "localhost:8080").is_none());
    }

    #[test]
    fn shared_reviews_expire_and_cannot_cross_sessions_series_or_plans() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost").unwrap();
        let session = sessions
            .login(&challenge.id, &challenge.csrf, "http://localhost", true)
            .unwrap()
            .unwrap();
        let query = crate::pack::SharedFileRequest {
            source_url: "magnet:?private=fixture".into(),
            file_path: "Pack/shared.mp4".into(),
            season: 1,
            episodes: vec![1, 2],
            apply: true,
            plan_id: Some("a".repeat(64)),
        };
        sessions
            .save_shared_preview(&session.id, "series", query.clone())
            .unwrap();
        assert_eq!(
            sessions
                .shared_preview(&session.id, "series", &"a".repeat(64))
                .unwrap(),
            query
        );
        assert!(
            sessions
                .shared_preview("other-session", "series", &"a".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .shared_preview(&session.id, "other-series", &"a".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .shared_preview(&session.id, "series", &"b".repeat(64))
                .is_err()
        );
        sessions
            .0
            .get_mut(&session.id)
            .unwrap()
            .shared_preview
            .as_mut()
            .unwrap()
            .expires = Instant::now();
        assert!(
            sessions
                .shared_preview(&session.id, "series", &"a".repeat(64))
                .is_err()
        );
        sessions
            .save_shared_preview(&session.id, "series", query)
            .unwrap();
        sessions.clear_shared_preview(&session.id);
        assert!(
            sessions
                .shared_preview(&session.id, "series", &"a".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn session_capacity_and_failed_login_attempts_are_bounded() {
        let mut sessions = Sessions::new();
        for _ in 0..MAX_SESSIONS + 1 {
            sessions.challenge("localhost").unwrap();
        }
        assert_eq!(sessions.0.len(), MAX_SESSIONS);
        let challenge = sessions.challenge("localhost").unwrap();
        for _ in 0..5 {
            assert!(
                sessions
                    .login(&challenge.id, &challenge.csrf, "http://localhost:80", false)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(sessions.get(&challenge.id, "localhost").is_none());
        sessions.challenge("localhost").unwrap();
        for session in sessions.0.values_mut() {
            session.origin = Some("http://localhost:80".into());
        }
        assert!(sessions.challenge("localhost").is_err());
    }

    #[test]
    fn group_review_expires_and_never_transfers_to_another_owner_or_session() {
        let mut sessions = Sessions::new();
        let challenge = sessions.challenge("localhost:8080").unwrap();
        let session = sessions
            .login(
                &challenge.id,
                &challenge.csrf,
                "http://localhost:8080",
                true,
            )
            .unwrap()
            .unwrap();
        let query = crate::library::GroupRequest {
            action: "replace".into(),
            release_title: "Fixture.Series.S01.1080p.WEB-DL".into(),
            source_url: Some("magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            file_path: Some("Pack/shared.mp4".into()),
            apply: true,
            plan_id: Some("a".repeat(64)),
        };
        sessions
            .save_group_preview(&session.id, "owner", query.clone())
            .unwrap();
        assert_eq!(
            sessions
                .group_preview(&session.id, "owner", &"a".repeat(64))
                .unwrap(),
            query
        );
        assert!(
            sessions
                .group_preview(&session.id, "other-owner", &"a".repeat(64))
                .is_err()
        );
        assert!(
            sessions
                .group_preview("other-session", "owner", &"a".repeat(64))
                .is_err()
        );
        sessions
            .0
            .get_mut(&session.id)
            .unwrap()
            .group_preview
            .as_mut()
            .unwrap()
            .expires = Instant::now();
        assert!(
            sessions
                .group_preview(&session.id, "owner", &"a".repeat(64))
                .is_err()
        );
        sessions
            .save_group_preview(&session.id, "owner", query)
            .unwrap();
        sessions.clear_group_preview(&session.id);
        assert!(
            sessions
                .group_preview(&session.id, "owner", &"a".repeat(64))
                .is_err()
        );
    }
}
