//! DeepSeek: Chat Completions and Responses.

use respan_gateway_conversation::{Content, Input, Output, Role};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Filter, NativeFields};
use serde_json::{Map, json};

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor, chat};

/// Where its API is.
const BASE_URL: &str = "https://api.deepseek.com";

/// DeepSeek.
pub const VENDOR: Vendor = Vendor {
    id: "deepseek",
    key_env: &["DEEPSEEK_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, DeepSeek), Some(BASE_URL)),
        (
            &Dialected::new(OpenAiResponses, DeepSeekResponses),
            Some(BASE_URL),
        ),
    ],
    ..Vendor::DEFAULT
};

/// DeepSeek's Chat Completions: it takes reasoning back as
/// `reasoning_content`.
#[derive(Debug, Clone, Copy)]
pub struct DeepSeek;

impl Dialect for DeepSeek {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(chat::write_reasoning))
    }
}

/// DeepSeek's Responses: an assistant's reasoning must return as a
/// non-empty `reasoning_text` part, not only as the summary another
/// compatible provider may return.
#[derive(Debug, Clone, Copy)]
pub struct DeepSeekResponses;

impl Dialect for DeepSeekResponses {
    type Input = Input;
    type Output = Output;

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(write_responses_reasoning))
    }
}

/// Restore `reasoning_text` from the reasoning text the gateway carries.
///
/// A Command Code response can expose thinking in `summary` while leaving
/// `content` empty. DeepSeek rejects the next tool-call turn unless the
/// same history reaches it as a non-empty `reasoning_text` part.
fn write_responses_reasoning(input: &mut Input) -> Result<(), respan_gateway_core::Fault> {
    for message in &mut input.messages {
        if message.role != Role::Assistant {
            continue;
        }
        for part in &mut message.content {
            let Content::Reasoning { text, native } = part else {
                continue;
            };
            let content_text = native
                .as_ref()
                .filter(|fields| &*fields.format == OpenAiResponses::NAME)
                .and_then(reasoning_content_text);
            if text.is_empty()
                && let Some(content_text) = &content_text
            {
                *text = content_text.clone();
            }
            if content_text.is_some() {
                continue;
            }
            if text.is_empty() {
                continue;
            }
            let content = json!([{ "type": "reasoning_text", "text": text.as_str() }]);
            match native {
                Some(fields) if &*fields.format == OpenAiResponses::NAME => {
                    fields.fields.insert("content".into(), content);
                }
                _ => {
                    *native = Some(NativeFields::new(
                        OpenAiResponses::NAME,
                        Map::from_iter([("content".into(), content)]),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The non-empty `reasoning_text` parts of a Responses reasoning item.
fn reasoning_content_text(fields: &NativeFields) -> Option<String> {
    let content = fields.fields.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter(|part| part["type"] == "reasoning_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn summary_only_reasoning_returns_as_reasoning_text() {
        let mut input = OpenAiResponses.decode_json(&json!({
            "model": "deepseek-flash",
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{ "type": "summary_text", "text": "think" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "tool",
                    "arguments": "{}"
                }
            ]
        }));
        input.model = Some("deepseek-flash".into());

        write_responses_reasoning(&mut input).expect("written");

        let sent = OpenAiResponses
            .encode_request(&input)
            .map(Value::from)
            .expect("a request");
        assert_eq!(
            sent["input"][0]["content"],
            json!([{ "type": "reasoning_text", "text": "think" }])
        );
    }

    #[test]
    fn existing_reasoning_text_is_kept() {
        let mut input = OpenAiResponses.decode_json(&json!({
            "model": "deepseek-flash",
            "input": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": "summary" }],
                "content": [{ "type": "reasoning_text", "text": "exact thinking" }]
            }]
        }));
        input.model = Some("deepseek-flash".into());

        write_responses_reasoning(&mut input).expect("written");

        let sent = OpenAiResponses
            .encode_request(&input)
            .map(Value::from)
            .expect("a request");
        assert_eq!(
            sent["input"][0]["content"],
            json!([{ "type": "reasoning_text", "text": "exact thinking" }])
        );
    }

    #[test]
    fn an_empty_reasoning_item_is_not_invented() {
        let mut input = OpenAiResponses.decode_json(&json!({
            "model": "deepseek-flash",
            "input": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": []
            }]
        }));
        input.model = Some("deepseek-flash".into());

        write_responses_reasoning(&mut input).expect("written");

        let sent = OpenAiResponses
            .encode_request(&input)
            .map(Value::from)
            .expect("a request");
        assert!(sent["input"][0].get("content").is_none());
    }
}
