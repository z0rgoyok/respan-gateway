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

use chrono::DateTime;
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
    checked_at: Option<SystemTime>,
    org_id: Option<String>,
    reset_at: Option<SystemTime>,
    quota: Option<Quota>,
    error: Option<String>,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            availability: Availability::Unknown,
            checked: None,
            checked_at: None,
            org_id: None,
            reset_at: None,
            quota: None,
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
                    checked_at: probe.checked_at,
                    reset_at: probe.reset_at,
                    quota: probe.quota,
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
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderStatus {
    /// Its provider name in routes.
    pub name: String,
    /// Whether it can be selected now.
    pub available: bool,
    /// When the latest successful credits check completed.
    pub checked_at: Option<SystemTime>,
    /// When an exhausted rolling window resets, when known.
    pub reset_at: Option<SystemTime>,
    /// The latest rolling-window and credit values returned by Command Code.
    pub quota: Option<Quota>,
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
            for change in changes {
                notice(&change);
            }
            join_all(
                inner
                    .providers
                    .iter()
                    .map(|provider| inner.refresh_month(provider)),
            )
            .await;
            drop(inner);
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
            Ok((value, org_id)) => {
                let now = SystemTime::now();
                let (availability, reset_at) = value.availability(now);
                let mut quota = value.details();
                if let Some(previous) = self.state.read().get(&provider.name)
                    && previous.org_id == org_id
                {
                    quota.monthly = previous
                        .quota
                        .as_ref()
                        .and_then(|quota| quota.monthly.clone());
                }
                Probe {
                    availability,
                    checked: Some(Instant::now()),
                    checked_at: Some(now),
                    org_id,
                    reset_at,
                    quota: Some(quota),
                    error: None,
                }
            }
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
    ) -> std::result::Result<(CreditsResponse, Option<String>), String> {
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
        let credits_url = self.with_org("/alpha/billing/credits", org_id.as_deref())?;
        let value: CreditsResponse =
            self.get_value(credits_url, &provider.key)
                .await
                .and_then(|value| {
                    serde_json::from_value(value)
                        .map_err(|_| "quota endpoint returned an unexpected response".to_owned())
                })?;
        Ok((value, org_id))
    }

    async fn refresh_month(&self, provider: &Provider) {
        let (checked_at, org_id) = {
            let state = self.state.read();
            let Some(probe) = state
                .get(&provider.name)
                .filter(|probe| probe.error.is_none() && probe.quota.is_some())
            else {
                return;
            };
            (probe.checked_at, probe.org_id.clone())
        };
        let (Ok(subscription), Ok(usage)) = (
            self.with_org("/alpha/billing/subscriptions", org_id.as_deref()),
            self.with_org("/alpha/usage/summary", org_id.as_deref()),
        ) else {
            return;
        };
        let (subscription, usage) = respan_gateway_core::futures::future::join(
            self.get_value(subscription, &provider.key),
            self.get_value(usage, &provider.key),
        )
        .await;
        if let Some(probe) = self.state.write().get_mut(&provider.name)
            && probe.checked_at == checked_at
            && let Some(quota) = &mut probe.quota
        {
            quota.monthly = monthly_window(
                quota.monthly_remaining,
                subscription.ok().as_ref(),
                usage.ok().as_ref(),
            );
        }
    }

    fn with_org(&self, path: &str, org_id: Option<&str>) -> std::result::Result<Url, String> {
        let mut url = self.endpoint(path)?;
        if let Some(org_id) = org_id {
            url.query_pairs_mut().append_pair("orgId", org_id);
        }
        Ok(url)
    }

    async fn get_value(
        &self,
        url: Url,
        key: &str,
    ) -> std::result::Result<serde_json::Value, String> {
        let response = self
            .http
            .get(url)
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

    async fn get(&self, path: &str, key: &str) -> std::result::Result<serde_json::Value, String> {
        self.get_value(self.endpoint(path)?, key).await
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

    fn details(&self) -> Quota {
        Quota {
            five_hour: self.window_limits.five_hour.as_ref().map(Window::quota),
            weekly: self.window_limits.weekly.as_ref().map(Window::quota),
            monthly: None,
            monthly_remaining: self.credits.monthly_credits,
            purchased_remaining: self.credits.purchased_credits,
        }
    }
}

fn monthly_window(
    remaining: f64,
    subscription: Option<&serde_json::Value>,
    usage: Option<&serde_json::Value>,
) -> Option<QuotaWindow> {
    let used = usage?.get("totalMonthlyCredits")?.as_f64()?;
    let reset_at = subscription
        .and_then(|value| value.get("data"))
        .and_then(|value| value.get("currentPeriodEnd"))
        .and_then(serde_json::Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(SystemTime::from);
    Some(QuotaWindow {
        used: Some(used),
        cap: Some(used + remaining),
        reset_at,
    })
}

/// A rolling or billing window returned by Command Code.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaWindow {
    /// Credits consumed in the window, when the API reports it.
    pub used: Option<f64>,
    /// Credits available in the window, when the API reports it.
    pub cap: Option<f64>,
    /// When the window resets, when known.
    pub reset_at: Option<SystemTime>,
}

impl Window {
    fn quota(&self) -> QuotaWindow {
        QuotaWindow {
            used: self.used,
            cap: self.cap,
            reset_at: self.reset_at.and_then(unix_ms),
        }
    }
}

/// The latest non-secret quota data for one Command Code account.
#[derive(Clone, Debug, PartialEq)]
pub struct Quota {
    /// The five-hour rolling window.
    pub five_hour: Option<QuotaWindow>,
    /// The weekly rolling window.
    pub weekly: Option<QuotaWindow>,
    /// The current billing month.
    pub monthly: Option<QuotaWindow>,
    /// Credits remaining in the current month.
    pub monthly_remaining: f64,
    /// Purchased credits remaining.
    pub purchased_remaining: f64,
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
    #[serde(default)]
    used: Option<f64>,
    #[serde(default)]
    cap: Option<f64>,
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
    fn quota_details_keep_windows_and_monthly_reset() {
        let value: CreditsResponse = serde_json::from_value(serde_json::json!({
            "credits": {"monthlyCredits": 12.5, "purchasedCredits": 1.25, "freeCredits": 0.5},
            "windowLimits": {
                "exceeded": null,
                "fiveHour": {"used": 2.0, "cap": 10.0, "resetAt": 1900000000000_u64},
                "weekly": {"used": 4.0, "cap": 20.0, "resetAt": 1900000000000_u64}
            }
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        let quota = value.details();
        let monthly = monthly_window(
            quota.monthly_remaining,
            Some(&serde_json::json!({"data": {"currentPeriodEnd": "2030-01-02T03:04:05.000Z"}})),
            Some(&serde_json::json!({"totalMonthlyCredits": 7.5})),
        )
        .expect("monthly usage");
        assert_eq!(quota.five_hour.as_ref().and_then(|w| w.cap), Some(10.0));
        assert_eq!(monthly.cap, Some(20.0));
        assert_eq!(
            monthly.reset_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_893_553_445))
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
                    ..Probe::default()
                },
            );
            state.insert(
                "cc2".to_owned(),
                Probe {
                    availability: Availability::Available,
                    checked,
                    reset_at: None,
                    error: None,
                    ..Probe::default()
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
