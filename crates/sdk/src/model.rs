//! A model's card: what a provider's list says of it, as one value.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Price;

/// A model, as its provider lists it or a host declares it: who serves it,
/// what kind it is, what it reads, can do and costs, and when it came and
/// goes. It serializes as a `/v1/models` entry's fields, `created` in
/// seconds since the epoch and `retires_at` in RFC 3339; it deserializes as
/// a host declares one, without its id and provider, which say where it is
/// declared, and with its times as RFC 3339 or a date.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct Model {
    /// Its id, the name a call takes.
    #[serde(skip_deserializing)]
    pub id: String,
    /// The provider that serves it.
    #[serde(rename = "owned_by", skip_deserializing)]
    pub provider: String,
    /// The kind of requests it serves, such as `chat`, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Who made it, such as `anthropic`, if its provider serves others'
    /// models and says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maker: Option<String>,
    /// When it was made, if it says.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "seconds",
        deserialize_with = "when"
    )]
    pub created: Option<DateTime<Utc>>,
    /// Its name for a person, such as `Claude Haiku 5.5`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How many tokens it reads at most.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// How many tokens it writes at most.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// What it reads beyond text, each once.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<Modality>,
    /// What it can do, each once.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    /// What it costs, if known.
    #[serde(rename = "pricing", skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// Source, verification date and conditional rates for this model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_card: Option<crate::RateCard>,
    /// When it is retired, if it is to be.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "rfc3339",
        deserialize_with = "when"
    )]
    pub retires_at: Option<DateTime<Utc>>,
}

fn seconds<S: Serializer>(at: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
    at.map(|t| t.timestamp()).serialize(s)
}

fn rfc3339<S: Serializer>(at: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
    at.map(|t| t.to_rfc3339()).serialize(s)
}

/// A time as RFC 3339 or a date ([`time`]).
fn when<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    let text = String::deserialize(d)?;
    match time(&text) {
        Some(at) => Ok(Some(at)),
        None => Err(serde::de::Error::custom(format!(
            "`{text}` is no RFC 3339 time or date"
        ))),
    }
}

impl Model {
    /// Model `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// It, with what `over` says laid over what it says: each of `over`'s
    /// fields that says something, as a host's declaration over a list's.
    pub fn overlay(&mut self, over: &Model) {
        fn take<T: Clone>(field: &mut Option<T>, over: &Option<T>) {
            if over.is_some() {
                field.clone_from(over);
            }
        }
        take(&mut self.kind, &over.kind);
        take(&mut self.maker, &over.maker);
        take(&mut self.created, &over.created);
        take(&mut self.display_name, &over.display_name);
        take(&mut self.description, &over.description);
        take(&mut self.context_window, &over.context_window);
        take(&mut self.max_output_tokens, &over.max_output_tokens);
        take(&mut self.price, &over.price);
        take(&mut self.rate_card, &over.rate_card);
        take(&mut self.retires_at, &over.retires_at);
        if !over.input_modalities.is_empty() {
            self.input_modalities.clone_from(&over.input_modalities);
        }
        if !over.capabilities.is_empty() {
            self.capabilities.clone_from(&over.capabilities);
        }
    }

    /// It reads `modality`, said once however often a list says it.
    pub fn reads(&mut self, modality: Modality) {
        if !self.input_modalities.contains(&modality) {
            self.input_modalities.push(modality);
        }
    }

    /// It can do `capability`, said once however often a list says it.
    pub fn can(&mut self, capability: Capability) {
        if !self.capabilities.contains(&capability) {
            self.capabilities.push(capability);
        }
    }

    /// Whether it is known to lack `need`, a capability or what it reads,
    /// by any name a vendor gives it (`vision` is an image, `pdf` a
    /// document). Each is judged only by a list that said some of its
    /// kind: one that names no capability lacks none, and likewise for
    /// what it reads.
    pub fn lacks(&self, need: &str) -> bool {
        let known = |said: bool, has: bool| !said || has;
        let can = Capability::parse(need).map(|c| {
            known(
                !self.capabilities.is_empty(),
                self.capabilities.contains(&c),
            )
        });
        let reads = Modality::parse(need).map(|m| {
            known(
                !self.input_modalities.is_empty(),
                self.input_modalities.contains(&m),
            )
        });
        [can, reads].contains(&Some(false)) && !(can == Some(true) || reads == Some(true))
    }
}

/// What a model reads beyond text, whatever a vendor calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Modality {
    /// Images.
    Image,
    /// Audio.
    Audio,
    /// Video.
    Video,
    /// Documents, such as PDFs.
    Document,
}

impl Modality {
    /// Its name: `image`, `audio`, `video` or `document`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Document => "document",
        }
    }

    /// A list's name for one: its own, or a vendor's (`vision` for an
    /// image, `file` or `pdf` for a document); none for text, which every
    /// model reads.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "image" | "images" | "vision" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            "document" | "file" | "pdf" => Some(Self::Document),
            _ => None,
        }
    }
}

/// What a model can do, whatever a vendor calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// It calls tools.
    Tools,
    /// It reasons before it answers.
    Reasoning,
    /// It answers in a JSON schema it is given.
    StructuredOutputs,
}

impl Capability {
    /// Its name: `tools`, `reasoning` or `structured_outputs`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Reasoning => "reasoning",
            Self::StructuredOutputs => "structured_outputs",
        }
    }

    /// A list's name for one: its own, or a vendor's (`function_calling`,
    /// `thinking`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "tools" | "function_calling" => Some(Self::Tools),
            "reasoning" | "thinking" => Some(Self::Reasoning),
            "structured_outputs" => Some(Self::StructuredOutputs),
            _ => None,
        }
    }
}

/// `text`, an RFC 3339 time (`2026-10-23T00:00:00Z`), or a date
/// (`2026-10-23`) as its midnight in UTC.
pub fn time(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
            Some(date.and_hms_opt(0, 0, 0)?.and_utc())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_card_is_a_models_list_entry() {
        let mut model = Model::new("gpt-x");
        model.provider = "openai".into();
        model.kind = Some("chat".into());
        model.created = DateTime::from_timestamp(1, 0);
        model.retires_at = time("2026-10-23");
        model.reads(Modality::Image);
        model.can(Capability::StructuredOutputs);
        model.price = Some(Price::new(1.0, 2.0, None));
        assert_eq!(
            serde_json::to_value(&model).ok(),
            Some(json!({
                "id": "gpt-x", "owned_by": "openai", "kind": "chat", "created": 1,
                "input_modalities": ["image"], "capabilities": ["structured_outputs"],
                "pricing": {"input": 1.0, "output": 2.0},
                "retires_at": "2026-10-23T00:00:00+00:00",
            }))
        );
    }

    #[test]
    fn a_need_is_met_by_any_vendors_name_of_it() {
        let mut model = Model::new("m");
        assert!(!model.lacks("tools"), "unknown: lacks nothing");
        model.reads(Modality::Document);
        assert!(!model.lacks("pdf"));
        assert!(model.lacks("vision"));
        assert!(!model.lacks("tools"), "no capability said: lacks none");
        model.can(Capability::Tools);
        assert!(!model.lacks("function_calling"));
        assert!(model.lacks("reasoning"));
        let mut gemini = Model::new("g");
        gemini.can(Capability::Reasoning);
        assert!(!gemini.lacks("vision"), "no modality said: reads any");
        assert!(gemini.lacks("tools"));
    }

    #[test]
    fn a_time_or_a_date_is_read_in_utc() {
        let at = |text| time(text).map(|t| t.timestamp());
        assert_eq!(at("2026-10-23"), Some(1_792_713_600));
        assert_eq!(at("2026-10-23T01:30:00.5+01:30"), Some(1_792_713_600));
        assert_eq!(at("soon"), None);
    }

    #[test]
    fn a_declared_card_lays_over_a_listed_one() {
        let declared: Model = serde_json::from_value(json!({
            "pricing": {"input": 2.0, "output": 8.0},
            "capabilities": ["tools"],
            "retires_at": "2027-01-01",
        }))
        .expect("a card");
        let mut listed = Model::new("m");
        listed.display_name = Some("M".into());
        listed.price = Some(Price::new(1.0, 1.0, None));
        listed.overlay(&declared);
        assert_eq!(listed.display_name.as_deref(), Some("M"));
        assert_eq!(listed.price.map(|p| p.input), Some(2.0));
        assert_eq!(listed.capabilities, [Capability::Tools]);
        assert_eq!(listed.retires_at, time("2027-01-01"));
        let refused = serde_json::from_value::<Model>(json!({"price": {}}));
        assert!(refused.is_err(), "an unknown field");
    }
}
