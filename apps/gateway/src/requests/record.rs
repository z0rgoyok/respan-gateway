//! A request as the admin API shows it, and the clipping of what a client
//! sent.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// The most characters of a client's path or model kept.
pub(super) const KEPT_CHARS: usize = 200;

/// One request, as the admin API returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub started_at: String,
    pub method: String,
    pub path: String,
    pub format: Option<String>,
    pub kind: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// The name of the client key it came with; absent without client keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub ttfb_ms: Option<u64>,
    pub stream: bool,
    pub usage: Option<Usage>,
    /// What it cost in dollars, by the config's `prices`; absent without a
    /// price for its model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Distinct upstream rate cards applied, bounded per request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_cards: Vec<serde_json::Value>,
    /// The calls to the model the tool loop made for it and the tools it
    /// ran, in order; none without the loop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    pub error: Option<String>,
}

/// One call to the model in a tool loop, and the tools it asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub index: u32,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Why the model stopped, such as `tool_use` or `stop`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
    pub tools: Vec<ToolCall>,
}

/// A tool the gateway ran for the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// As the model named it, `<server>__<tool>`.
    pub name: String,
    pub duration_ms: u64,
    pub error: bool,
    /// The bytes of its answer the model saw.
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Of `input_tokens`, those read from a cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_input_tokens: u64,
    /// Seconds of audio billed, as a transcription's provider may say
    /// instead of tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a client sent, as kept: its first [`KEPT_CHARS`] characters, and
/// `…` if there were more.
pub(super) fn clip(text: &str) -> String {
    match text.char_indices().nth(KEPT_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// `time` as RFC 3339 in UTC, to the millisecond.
pub fn rfc3339(time: SystemTime) -> String {
    let time = chrono::DateTime::<chrono::Utc>::from(time);
    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn times_are_rfc3339_with_millis() {
        let at = |secs: u64, millis: u64| {
            rfc3339(SystemTime::UNIX_EPOCH + Duration::from_millis(secs * 1000 + millis))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(1_791_633_600, 7), "2026-10-10T12:00:00.007Z");
        assert_eq!(at(951_782_400, 999), "2000-02-29T00:00:00.999Z");
    }

    #[test]
    fn what_a_client_sent_is_clipped() {
        let long = "m".repeat(10_000);
        let kept = clip(&long);
        assert_eq!(kept.chars().count(), KEPT_CHARS + 1);
        assert!(kept.ends_with('…'));
        assert_eq!(clip("gpt-4o"), "gpt-4o");
    }
}
