//! What a model costs.

use serde::{Deserialize, Serialize};

/// A model's price, in US dollars per million tokens, and per second of
/// audio for a model billed by its length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// Of each input token.
    pub input: f64,
    /// Of each output token.
    pub output: f64,
    /// Of each input token read from a cache; `input` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
    /// Of each input token written to a cache; `input` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Input rate for audio when billed separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<f64>,
    /// Cached audio input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_audio_input: Option<f64>,
    /// Output rate for image tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_output: Option<f64>,
    /// Of each second of audio, as a transcription model may bill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_second: Option<f64>,
}

impl Price {
    /// A price of `input` and `output`, and of `cached_input` if it has
    /// its own.
    pub fn new(input: f64, output: f64, cached_input: Option<f64>) -> Self {
        Self {
            input,
            output,
            cached_input,
            cache_write: None,
            ..Self::default()
        }
    }

    /// What a typical call weighs it at, to rank models by: input, and
    /// output three times, as calls write less than they read.
    pub fn weight(&self) -> f64 {
        self.input + 3.0 * self.output
    }
}

/// The official reference for a model and any conditional token rates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    /// Publisher's URL.
    pub source: String,
    /// ISO date of the verified tariff snapshot.
    pub verified_on: String,
    /// Scheduled end of a promotional tariff (UTC date, inclusive start of the next rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<ScheduledPrice>,
    /// Rates for a request above a prompt threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContext>,
    /// Discount outside the supplier's UTC peak windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_peak: Option<OffPeak>,
}

/// Replacement base price from a published date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledPrice {
    /// ISO UTC date the new price takes effect.
    pub from: String,
    /// Replacement token rates.
    pub pricing: Price,
}

/// A long prompt changes the rates for the whole request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongContext {
    /// Exclusive threshold including cached tokens.
    pub above_input_tokens: u64,
    /// Applicable rates.
    pub pricing: Price,
}

/// Off-peak schedule; windows are half-open UTC hours Monday to Friday.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffPeak {
    /// Weekday peak periods.
    pub peak_hours_utc: Vec<[u8; 2]>,
    /// Publisher's holiday dates that are off-peak all day.
    pub holidays: Vec<String>,
    /// Applicable discounted rates.
    pub pricing: Price,
}

impl RateCard {
    /// Choose the applicable rates at admission time.
    pub fn rates(&self, base: Price, input_tokens: u64, at: std::time::SystemTime) -> Price {
        use chrono::{DateTime, Datelike, Timelike, Utc};
        let utc: DateTime<Utc> = at.into();
        if let Some(next) = &self.next
            && utc.date_naive().to_string() >= next.from
        {
            return next.pricing;
        }
        if let Some(long) = &self.long_context
            && input_tokens > long.above_input_tokens
        {
            return long.pricing;
        }
        if let Some(off) = &self.off_peak {
            let hour = utc.hour() as u8;
            if utc.weekday().number_from_monday() > 5
                || !off
                    .peak_hours_utc
                    .iter()
                    .any(|[from, to]| *from <= hour && hour < *to)
                || off.holidays.contains(&utc.date_naive().to_string())
            {
                return off.pricing;
            }
        }
        base
    }
}
