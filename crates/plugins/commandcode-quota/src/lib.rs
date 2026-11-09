#![warn(missing_docs)]
//! Route around exhausted Command Code accounts.
//!
//! The plugin polls the plan endpoints used by Command Code's `/usage` UI and
//! keeps a cache per configured provider. It runs after routing has planned a
//! rule and before dispatch sends its candidates, reordering only the accounts
//! it knows about while preserving the rule's other fallbacks.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use http::StatusCode;
use parking_lot::RwLock;
use reqwest::{Client, Url};
use respan_gateway_core::futures::future::join_all;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Plugin, ProviderContext, Response, Result, RetryAfter,
    async_trait,
};
use respan_gateway_http_faults::Status;
use respan_gateway_plugin_routing::{Chosen, Planned};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

/// Command Code's provider API origin.
pub const DEFAULT_URL: &str = "https://api.commandcode.ai";
/// How often quota is refreshed unless configured otherwise.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// One Command Code provider name and its API key.
#[derive(Clone)]
pub struct Provider {
    name: String,
    key: String,
}

impl Provider {
    /// A provider named in routes, with the key sent to Command Code.
    pub fn new(name: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            key: key.into(),
        }
    }
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Availability {
    Unknown,
    Available,
    Exhausted,
}

#[derive(Clone, Debug)]
struct Probe {
    availability: Availability,
    checked: Option<Instant>,
    reset_at: Option<SystemTime>,
    error: Option<String>,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            availability: Availability::Unknown,
            checked: None,
            reset_at: None,
            error: None,
        }
    }
}

/// A quota-aware router for Command Code accounts.
#[derive(Clone)]
pub struct CommandCodeQuota {
    inner: Arc<Inner>,
}

impl fmt::Debug for CommandCodeQuota {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandCodeQuota")
            .field("providers", &self.inner.providers)
            .finish_non_exhaustive()
    }
}

struct Inner {
    url: Url,
    providers: Vec<Provider>,
    names: HashSet<String>,
    state: RwLock<HashMap<String, Probe>>,
    http: Client,
    interval: Duration,
    cursor: AtomicUsize,
    refreshing: AsyncMutex<()>,
}

/// A background quota poller that ends when its plugin is no longer retained.
#[derive(Clone, Debug)]
pub struct Worker {
    inner: Weak<Inner>,
}

impl CommandCodeQuota {
    /// Build the plugin. `url` is Command Code's origin and `http` applies the
    /// gateway's configured proxy and certificate policy.
    pub fn new(
        url: &str,
        providers: Vec<Provider>,
        interval: Duration,
        http: Client,
    ) -> Result<Self> {
        let url = Url::parse(url)
            .map_err(|_| Fault::config("commandcode_quota: url is not a valid HTTP URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Fault::config(
                "commandcode_quota: url must use http or https",
            ));
        }
        if providers.is_empty() {
            return Err(Fault::config(
                "commandcode_quota: at least one provider is required",
            ));
        }
        if interval.is_zero() {
            return Err(Fault::config(
                "commandcode_quota: poll_interval must be positive",
            ));
        }
        let mut names = HashSet::new();
        let mut state = HashMap::new();
        for provider in &providers {
            if provider.name.is_empty() || provider.key.is_empty() {
                return Err(Fault::config(
                    "commandcode_quota: provider name and key must be non-empty",
                ));
            }
            if !names.insert(provider.name.clone()) {
                return Err(Fault::config(format!(
                    "commandcode_quota: duplicate provider '{}'",
                    provider.name
                )));
            }
            state.insert(provider.name.clone(), Probe::default());
        }
        Ok(Self {
            inner: Arc::new(Inner {
                url,
                providers,
                names,
                state: RwLock::new(state),
                http,
                interval,
                cursor: AtomicUsize::new(0),
                refreshing: AsyncMutex::new(()),
            }),
        })
    }

    /// A worker to spawn on the gateway runtime.
    pub fn worker(&self) -> Worker {
        Worker {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// The current safe status of each configured provider.
    pub fn statuses(&self) -> Vec<ProviderStatus> {
        let state = self.inner.state.read();
        self.inner
            .providers
            .iter()
            .map(|provider| {
                let probe = state.get(&provider.name).cloned().unwrap_or_default();
                ProviderStatus {
                    name: provider.name.clone(),
                    available: self.available(&probe, SystemTime::now()),
                    reset_at: probe.reset_at,
                    error: probe.error,
                }
            })
            .collect()
    }

    fn available(&self, probe: &Probe, now: SystemTime) -> bool {
        match probe.availability {
            Availability::Unknown | Availability::Available => true,
            Availability::Exhausted => probe.reset_at.is_some_and(|reset| reset <= now),
        }
    }

    async fn refresh_if_due(&self) {
        let due = {
            let state = self.inner.state.read();
            state.values().any(|probe| {
                probe
                    .checked
                    .is_none_or(|checked| checked.elapsed() >= self.inner.interval)
            })
        };
        if due {
            let _refreshing = self.inner.refreshing.lock().await;
            let still_due = {
                let state = self.inner.state.read();
                state.values().any(|probe| {
                    probe
                        .checked
                        .is_none_or(|checked| checked.elapsed() >= self.inner.interval)
                })
            };
            if still_due {
                self.inner.refresh_now().await;
            }
        }
    }
}

/// The safe current state of one configured Command Code provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderStatus {
    /// Its provider name in routes.
    pub name: String,
    /// Whether it can be selected now.
    pub available: bool,
    /// When an exhausted rolling window resets, when known.
    pub reset_at: Option<SystemTime>,
    /// Why the last probe failed.
    pub error: Option<String>,
}

impl Worker {
    /// Poll immediately, then at the configured interval. `notice` receives
    /// human-readable transitions; the worker stops with `stopping` or once
    /// its plugin is dropped.
    pub async fn run<F>(self, mut stopping: tokio::sync::watch::Receiver<bool>, notice: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        loop {
            if *stopping.borrow() {
                return;
            }
            let Some(inner) = self.inner.upgrade() else {
                return;
            };
            let interval = inner.interval;
            let changes = inner.refresh().await;
            drop(inner);
            for change in changes {
                notice(&change);
            }
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                changed = stopping.changed() => {
                    if changed.is_err() || *stopping.borrow() {
                        return;
                    }
                }
            }
        }
    }
}

impl Inner {
    async fn refresh(self: &Arc<Self>) -> Vec<String> {
        let Ok(_refreshing) = self.refreshing.try_lock() else {
            return Vec::new();
        };
        self.refresh_now().await
    }

    async fn refresh_now(self: &Arc<Self>) -> Vec<String> {
        let mut changes = Vec::new();
        let probes = join_all(
            self.providers
                .iter()
                .map(|provider| async move { (provider, self.probe(provider).await) }),
        )
        .await;
        for (provider, probe) in probes {
            let before = self
                .state
                .write()
                .insert(provider.name.clone(), probe.clone());
            let before = before.unwrap_or_default();
            if before.availability != probe.availability {
                changes.push(status_change(provider, &before, &probe));
            } else if before.error.is_some() && probe.error.is_none() {
                changes.push(format!(
                    "commandcode_quota: {}: quota probing recovered",
                    provider.name
                ));
            } else if probe.error != before.error
                && let Some(error) = &probe.error
            {
                changes.push(format!("commandcode_quota: {}: {error}", provider.name));
            }
        }
        changes
    }

    async fn probe(&self, provider: &Provider) -> Probe {
        match self.probe_inner(provider).await {
            Ok((availability, reset_at)) => Probe {
                availability,
                checked: Some(Instant::now()),
                reset_at,
                error: None,
            },
            Err(error) => {
                let mut previous = self
                    .state
                    .read()
                    .get(&provider.name)
                    .cloned()
                    .unwrap_or_default();
                previous.checked = Some(Instant::now());
                previous.error = Some(error);
                previous
            }
        }
    }

    async fn probe_inner(
        &self,
        provider: &Provider,
    ) -> std::result::Result<(Availability, Option<SystemTime>), String> {
        let who = self.get("/alpha/whoami?limits=1", &provider.key).await?;
        let org_id = who
            .get("org")
            .and_then(serde_json::Value::as_object)
            .and_then(|org| org.get("id"))
            .and_then(|id| match id {
                serde_json::Value::String(value) => Some(value.clone()),
                serde_json::Value::Number(value) => Some(value.to_string()),
                _ => None,
            });
        let credits_url = match org_id {
            Some(org_id) => {
                let mut url = self.endpoint("/alpha/billing/credits")?;
                url.query_pairs_mut().append_pair("orgId", &org_id);
                url
            }
            None => self.endpoint("/alpha/billing/credits")?,
        };
        let response = self
            .http
            .get(credits_url)
            .bearer_auth(&provider.key)
            .header("Accept", "application/json")
            .header("User-Agent", "respan-gateway-commandcode-quota/1")
            .send()
            .await
            .map_err(|error| safe_http_error(&error))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| safe_http_error(&error))?;
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        let value: CreditsResponse = serde_json::from_slice(&body)
            .map_err(|_| "quota endpoint returned an unexpected response".to_owned())?;
        Ok(value.availability(SystemTime::now()))
    }

    async fn get(&self, path: &str, key: &str) -> std::result::Result<serde_json::Value, String> {
        let response = self
            .http
            .get(self.endpoint(path)?)
            .bearer_auth(key)
            .header("Accept", "application/json")
            .header("User-Agent", "respan-gateway-commandcode-quota/1")
            .send()
            .await
            .map_err(|error| safe_http_error(&error))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| safe_http_error(&error))?;
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        serde_json::from_slice(&body)
            .map_err(|_| "quota endpoint returned an unexpected response".to_owned())
    }

    fn endpoint(&self, path: &str) -> std::result::Result<Url, String> {
        self.url
            .join(path.trim_start_matches('/'))
            .map_err(|_| "quota URL is invalid".to_owned())
    }

    fn ordered(&self, plan: &Planned, now: SystemTime) -> PlanDecision {
        let state = self.state.read();
        let mut managed = Vec::new();
        let mut fallback = Vec::new();
        for candidate in &plan.to {
            if self.names.contains(&candidate.provider) {
                managed.push(candidate.clone());
            } else {
                fallback.push(candidate.clone());
            }
        }
        if managed.is_empty() {
            return PlanDecision::Unchanged;
        }
        let mut available: Vec<Chosen> = managed
            .into_iter()
            .filter(|candidate| {
                let probe = state.get(&candidate.provider).cloned().unwrap_or_default();
                match probe.availability {
                    Availability::Unknown | Availability::Available => true,
                    Availability::Exhausted => probe.reset_at.is_some_and(|reset| reset <= now),
                }
            })
            .collect();
        drop(state);
        let mut to = Vec::with_capacity(plan.to.len());
        if !available.is_empty() {
            let start = self.cursor.fetch_add(1, Ordering::Relaxed) % available.len();
            available.rotate_left(start);
            to.extend(available);
        }
        to.extend(fallback);
        if to.is_empty() {
            return PlanDecision::Exhausted;
        }
        let mut plan = plan.clone();
        plan.to = to;
        PlanDecision::Change(plan)
    }

    fn mark_429(&self, provider: &str, retry_after: Option<Duration>) {
        if !self.names.contains(provider) {
            return;
        }
        let mut state = self.state.write();
        let probe = state.entry(provider.to_owned()).or_default();
        probe.availability = Availability::Exhausted;
        probe.checked = Some(Instant::now());
        probe.reset_at = retry_after.map(|wait| SystemTime::now() + wait);
    }

    fn mark_available(&self, provider: &str) {
        if !self.names.contains(provider) {
            return;
        }
        let mut state = self.state.write();
        let probe = state.entry(provider.to_owned()).or_default();
        probe.availability = Availability::Available;
        probe.error = None;
    }
}

enum PlanDecision {
    Unchanged,
    Change(Planned),
    Exhausted,
}

#[async_trait]
impl Plugin for CommandCodeQuota {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        if cx.extensions().get::<Arc<Planned>>().is_none() {
            return Ok(None);
        }
        self.refresh_if_due().await;
        let Some(plan) = cx.extensions().get::<Arc<Planned>>().cloned() else {
            return Ok(None);
        };
        match self.inner.ordered(&plan, SystemTime::now()) {
            PlanDecision::Unchanged => Ok(None),
            PlanDecision::Exhausted => Err(Fault::upstream(
                "every Command Code account has exhausted its quota",
            )
            .with(ErrorCode("commandcode_quota_exhausted".into()))
            .with(Status(StatusCode::SERVICE_UNAVAILABLE))),
            PlanDecision::Change(plan) => {
                if let Some(first) = plan.to.first().cloned() {
                    cx.set_target(first.internal.clone());
                    cx.extensions_mut().insert(first);
                    cx.extensions_mut().insert(Arc::new(plan));
                }
                Ok(None)
            }
        }
    }

    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<()>) {
        let Some(chosen) = px.extensions().get::<Chosen>() else {
            return;
        };
        match end {
            Ok(()) => self.inner.mark_available(&chosen.provider),
            Err(fault)
                if fault
                    .get::<Status>()
                    .is_some_and(|status| status.0 == StatusCode::TOO_MANY_REQUESTS) =>
            {
                self.inner.mark_429(
                    &chosen.provider,
                    fault.get::<RetryAfter>().map(|wait| wait.0),
                );
            }
            Err(_) => {}
        }
    }
}

fn status_change(provider: &Provider, before: &Probe, after: &Probe) -> String {
    match after.availability {
        Availability::Available => format!("commandcode_quota: {} available", provider.name),
        Availability::Exhausted => format!(
            "commandcode_quota: {} exhausted{}",
            provider.name,
            after
                .reset_at
                .map(|reset| format!(" until {reset:?}"))
                .unwrap_or_default()
        ),
        Availability::Unknown if before.availability != Availability::Unknown => {
            format!("commandcode_quota: {} quota is unknown", provider.name)
        }
        Availability::Unknown => String::new(),
    }
}

fn safe_http_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        "request failed".to_owned()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditsResponse {
    credits: Credits,
    window_limits: WindowLimits,
}

impl CreditsResponse {
    fn availability(&self, now: SystemTime) -> (Availability, Option<SystemTime>) {
        let exceeded = self.window_limits.exceeded.as_deref();
        let window = match exceeded {
            Some("five_hour") | Some("fiveHour") => self.window_limits.five_hour.as_ref(),
            Some("weekly") => self.window_limits.weekly.as_ref(),
            Some(_) => self.window_limits.weekly.as_ref(),
            None => None,
        };
        let reset_at = window.and_then(|window| window.reset_at).and_then(unix_ms);
        let has_credits = self.credits.monthly_credits > 0.0
            || self.credits.purchased_credits > 0.0
            || self.credits.free_credits > 0.0;
        if has_credits
            && (exceeded.is_none()
                || self.credits.purchased_credits > 0.0
                || reset_at.is_some_and(|reset| reset <= now))
        {
            (Availability::Available, reset_at)
        } else {
            (Availability::Exhausted, reset_at)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credits {
    #[serde(default)]
    monthly_credits: f64,
    #[serde(default)]
    purchased_credits: f64,
    #[serde(default)]
    free_credits: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WindowLimits {
    #[serde(default)]
    exceeded: Option<String>,
    #[serde(default)]
    five_hour: Option<Window>,
    #[serde(default)]
    weekly: Option<Window>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    reset_at: Option<u64>,
}

fn unix_ms(value: u64) -> Option<SystemTime> {
    (value > 0).then(|| UNIX_EPOCH + Duration::from_millis(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Input;
    use respan_gateway_conversation_http::openai::OpenAiChat;
    use respan_gateway_core::{Gateway, Provider as CoreProvider};
    use respan_gateway_http::http_body_util::BodyExt;
    use respan_gateway_http::{Body, http};
    use respan_gateway_http_transport::HttpTransport;
    use respan_gateway_plugin_routing::{By, Candidate, Rule};
    use respan_gateway_plugin_routing::{Dispatch, Model, Routes, Routing, Speaks};

    fn quota() -> CommandCodeQuota {
        CommandCodeQuota::new(
            DEFAULT_URL,
            ["cc1", "cc2", "cc3"]
                .into_iter()
                .map(|name| Provider::new(name, "test-key"))
                .collect(),
            DEFAULT_INTERVAL,
            Client::new(),
        )
        .unwrap_or_else(|error| panic!("{error}"))
    }

    fn planned(providers: &[&str]) -> Planned {
        let to = providers
            .iter()
            .map(|provider| Chosen {
                provider: (*provider).to_owned(),
                internal: (*provider).to_owned(),
                model: Some("model".to_owned()),
            })
            .collect();
        Planned {
            by: By::Rule(Arc::new(Rule::new(
                "model",
                providers
                    .iter()
                    .map(|provider| Candidate::parse(provider))
                    .collect(),
            ))),
            to,
            passed: Vec::new(),
        }
    }

    #[test]
    fn a_clean_window_is_available() {
        let value: CreditsResponse = serde_json::from_value(serde_json::json!({
            "credits": {"monthlyCredits": 10.0, "purchasedCredits": 0.0},
            "windowLimits": {
                "exceeded": null,
                "fiveHour": {"resetAt": 0},
                "weekly": {"resetAt": 0}
            }
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value.availability(SystemTime::now()).0,
            Availability::Available
        );
    }

    #[test]
    fn purchased_credits_keep_an_exceeded_window_available() {
        let value: CreditsResponse = serde_json::from_value(serde_json::json!({
            "credits": {"monthlyCredits": 0.0, "purchasedCredits": 9.5},
            "windowLimits": {
                "exceeded": "weekly",
                "weekly": {"resetAt": 1_900_000_000_000_u64}
            }
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value.availability(SystemTime::now()).0,
            Availability::Available
        );
    }

    #[test]
    fn an_exhausted_window_without_purchased_credits_is_unavailable() {
        let value: CreditsResponse = serde_json::from_value(serde_json::json!({
            "credits": {"monthlyCredits": 34.9, "purchasedCredits": 0.0},
            "windowLimits": {
                "exceeded": "weekly",
                "weekly": {"resetAt": 1_900_000_000_000_u64}
            }
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value.availability(SystemTime::now()).0,
            Availability::Exhausted
        );
    }

    #[test]
    fn exhausted_accounts_leave_the_rotation_and_fallbacks_stay_last() {
        let quota = quota();
        quota.inner.state.write().insert(
            "cc3".to_owned(),
            Probe {
                availability: Availability::Exhausted,
                reset_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                ..Probe::default()
            },
        );
        let PlanDecision::Change(plan) = quota.inner.ordered(
            &planned(&["cc1", "cc2", "cc3", "direct"]),
            SystemTime::now(),
        ) else {
            panic!("route was not changed")
        };
        let names: Vec<&str> = plan
            .to
            .iter()
            .map(|candidate| candidate.provider.as_str())
            .collect();
        assert_eq!(names, ["cc1", "cc2", "direct"]);
    }

    #[test]
    fn available_accounts_rotate() {
        let quota = quota();
        for turn in 0..2 {
            let PlanDecision::Change(plan) = quota
                .inner
                .ordered(&planned(&["cc1", "cc2", "cc3"]), SystemTime::now())
            else {
                panic!("route was not changed")
            };
            assert_eq!(plan.to[0].provider, if turn == 0 { "cc1" } else { "cc2" });
        }
    }

    #[test]
    fn a_direct_fallback_is_used_when_every_account_is_exhausted() {
        let quota = quota();
        let mut state = quota.inner.state.write();
        for name in ["cc1", "cc2", "cc3"] {
            state.insert(
                name.to_owned(),
                Probe {
                    availability: Availability::Exhausted,
                    reset_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                    ..Probe::default()
                },
            );
        }
        drop(state);
        let PlanDecision::Change(plan) = quota.inner.ordered(
            &planned(&["cc1", "cc2", "cc3", "direct"]),
            SystemTime::now(),
        ) else {
            panic!("route was not changed")
        };
        assert_eq!(plan.to.len(), 1);
        assert_eq!(plan.to[0].provider, "direct");
    }

    #[tokio::test]
    async fn the_gateway_sends_a_request_only_to_the_available_account() {
        let first = mockllm::FakeOpenAi::start().await;
        let second = mockllm::FakeOpenAi::start().await;
        first.answer(mockllm::Answer::text("first"));
        second.answer(mockllm::Answer::text("second"));

        let transport = |fake: &mockllm::FakeOpenAi| -> Result<HttpTransport> {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "authorization",
                http::HeaderValue::from_static("Bearer test"),
            );
            HttpTransport::new(fake.base_url())?.with_headers(headers)
        };
        let speaks = |internal: &str| Speaks {
            format: "openai-chat".to_owned(),
            kind: "chat",
            internal: internal.to_owned(),
        };
        let routes = Routes::new()
            .provider("cc1", vec![speaks("cc1")])
            .provider("cc2", vec![speaks("cc2")])
            .rule(Rule::new(
                "deepseek",
                vec![Candidate::parse("cc1"), Candidate::parse("cc2")],
            ));
        let routing = Routing::new(Arc::new(routes)).format(
            "openai-chat",
            "chat",
            Model::of(Input::model, Input::set_model),
        );
        let quota = quota();
        {
            let mut state = quota.inner.state.write();
            let checked = Some(Instant::now());
            state.insert(
                "cc1".to_owned(),
                Probe {
                    availability: Availability::Exhausted,
                    checked,
                    reset_at: Some(SystemTime::now() + Duration::from_secs(3600)),
                    error: None,
                },
            );
            state.insert(
                "cc2".to_owned(),
                Probe {
                    availability: Availability::Available,
                    checked,
                    reset_at: None,
                    error: None,
                },
            );
        }
        let gateway = Gateway::new()
            .client::<Input, _, _>(OpenAiChat)
            .unwrap_or_else(|error| panic!("{error}"));
        let gateway = gateway
            .provider(CoreProvider::new(
                "cc1",
                OpenAiChat,
                transport(&first)
                    .unwrap_or_else(|error| panic!("{error}"))
                    .connect(),
            ))
            .unwrap_or_else(|error| panic!("{error}"));
        let gateway = gateway
            .provider(CoreProvider::new(
                "cc2",
                OpenAiChat,
                transport(&second)
                    .unwrap_or_else(|error| panic!("{error}"))
                    .connect(),
            ))
            .unwrap_or_else(|error| panic!("{error}"));
        let gateway = gateway
            .plugin(routing)
            .plugin(quota)
            .plugin(Dispatch::new(respan_gateway_http_faults::retryable));

        let mut request = http::Request::new(Body::full(
            r#"{"model":"deepseek","messages":[{"role":"user","content":"hi"}]}"#,
        ));
        *request.method_mut() = http::Method::POST;
        *request.uri_mut() = http::Uri::from_static("/v1/chat/completions");
        let (response, _) = respan_gateway_http::handle(&gateway, request, 32 << 20).await;
        assert_eq!(response.status(), StatusCode::OK);
        response
            .into_body()
            .collect()
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        gateway.idle().await;
        assert_eq!(first.requests().len(), 0);
        assert_eq!(second.requests().len(), 1);
    }
}
