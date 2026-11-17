//! Read-only config view with secrets redacted.

use super::Admin;
use crate::config::{Config, Source};
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use respan_gateway_http::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;

pub const PLACEHOLDER: &str = "…";

/// Whether `value`, at `path` in the config, is a secret: a client key
/// (the entry, or a named entry's `key`), a provider's key (also inside
/// `auth`), an MCP server's header, bearer key, OAuth client secret and
/// refresh token, environment value or argument (not the program), and a
/// URL that holds credentials (a server's, a provider's, a proxy's, an
/// OAuth token endpoint's or Respan's). An `env:NAME` reference is no secret; it is
/// shown as written.
fn is_secret(path: &[String], value: &Value) -> bool {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["client_keys", _] => value.is_string(),
        ["client_keys", _, "key"]
        | ["providers", _, "key"]
        | ["plugins", "respan", "key"]
        | ["plugins", "commandcode_quota", "providers", _, "key"]
        | ["providers", _, "auth", "key"]
        | ["tools", _, "headers", _]
        | ["tools", _, "auth", "bearer"]
        | [
            "tools",
            _,
            "auth",
            "oauth",
            "client_secret" | "refresh_token",
        ]
        | ["tools", _, "env", _]
        | ["tools", _, "args", _]
        | ["tools", _, "key"] => !value.is_null(),
        ["tools", _, "url"]
        | ["tools", _, "base_url"]
        | ["network", "proxy"]
        | ["providers" | "tools", _, "proxy"]
        | ["tools", _, "auth", "oauth", "token_url"]
        | ["plugins", "respan" | "commandcode_quota", "url"]
        | ["providers", _, "base_url"]
        | ["providers", _, "formats", _] => value
            .as_str()
            .is_some_and(|u| u == PLACEHOLDER || url_holds_secret(u)),
        _ => false,
    }
}

fn is_reference(value: &Value) -> bool {
    value.as_str().is_some_and(|s| s.starts_with("env:"))
}

/// A URL with credentials in it: a userinfo, a query or fragment, or a
/// path segment that looks like a token (24 or more characters of letters
/// and digits, with both).
fn url_holds_secret(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let token = |segment: &str| {
        let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        segment.len() >= 24
            && segment.chars().all(plain)
            && segment.chars().any(|c| c.is_ascii_digit())
            && segment.chars().any(|c| c.is_ascii_alphabetic())
    };
    authority.contains('@') || tail.contains(['?', '#']) || tail.split('/').any(token)
}

/// Calls `found` on each secret in `value`, which sits at `path`.
fn secrets(value: &mut Value, path: &mut Vec<String>, found: &mut dyn FnMut(&mut Value)) {
    if is_secret(path, value) {
        return found(value);
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                path.push(key.clone());
                secrets(child, path, found);
                path.pop();
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                path.push(i.to_string());
                secrets(child, path, found);
                path.pop();
            }
        }
        _ => {}
    }
}

/// The name a bare client key is logged under, and shown under here.
fn bare_name(index: usize) -> String {
    format!("key-{}", index + 1)
}

/// `config`'s bare literal client keys as `{ name: key-<n>, key }`, using the same names as the request log.
fn name_bare_keys(config: &mut Value) {
    let Some(Value::Array(items)) = config.get_mut("client_keys") else {
        return;
    };
    for (i, item) in items.iter_mut().enumerate() {
        if let Value::String(key) = item
            && !key.starts_with("env:")
        {
            let mut named = serde_json::Map::new();
            named.insert("name".into(), bare_name(i).into());
            named.insert("key".into(), std::mem::take(key).into());
            *item = Value::Object(named);
        }
    }
}

/// `config` with every secret replaced by [`PLACEHOLDER`]. A bare client
/// key is shown as a named one (`key-<n>`, the name it is logged under).
pub fn redact(mut config: Value) -> Value {
    name_bare_keys(&mut config);
    secrets(&mut config, &mut Vec::new(), &mut |secret| {
        if !is_reference(secret) {
            *secret = Value::String(PLACEHOLDER.to_owned());
        }
    });
    config
}

/// What the admin API shows of a config that has no file: its providers,
/// from the environment.
pub fn stand_in(config: &Config) -> Value {
    let providers = config.providers.iter().map(|(name, p)| {
        let mut provider = serde_json::Map::new();
        let mut put = |key: &str, value: &str| {
            provider.insert(key.into(), Value::String(value.to_owned()));
        };
        if let Some(vendor) = &p.vendor {
            put("vendor", vendor);
        }
        if let Some(key) = &p.key {
            put("key", key);
        }
        if let Some(base_url) = &p.base_url {
            put("base_url", base_url);
        }
        (name.clone(), Value::Object(provider))
    });
    let mut root = serde_json::Map::new();
    root.insert("providers".into(), Value::Object(providers.collect()));
    redact(Value::Object(root))
}

/// Read the current file, or the provider summary for a non-file source.
fn current(admin: &Admin) -> Result<Value, String> {
    match &admin.source {
        Source::File(path) => {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            toml::from_str(&text).map_err(|e| crate::config::toml_error(&text, &e))
        }
        _ => Ok(admin.stand_in.clone()),
    }
}

/// GET /ui/api/config. Reading never changes the file or running config.
pub async fn show(State(admin): State<Arc<Admin>>) -> Response {
    let rendered = current(&admin).and_then(|value| {
        let config = redact(value);
        let text = toml::to_string_pretty(&config).map_err(|e| e.to_string())?;
        Ok((config, text))
    });
    match rendered {
        Ok((config, text)) => {
            let (source, path) = admin.source_and_path();
            Json(json!({
                "source": source, "path": path, "placeholder": PLACEHOLDER,
                "text": text, "format": "toml", "config": config,
            }))
            .into_response()
        }
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"type": "config_unreadable", "message": message}})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Value {
        if text.trim_start().starts_with(['{', '[']) {
            let mut document: Value =
                toml::from_str(&format!("value = {text}")).unwrap_or_else(|e| panic!("{e}"));
            document["value"].take()
        } else {
            toml::from_str(text).unwrap_or_else(|e| panic!("{e}"))
        }
    }

    #[test]
    fn hides_a_proxy_password() {
        let shown = redact(text(
            "network = { proxy = \"http://u:pw@proxy:3128\" }\nproviders = { a = { proxy = \"http://u:pw@p:1\" }, b = { proxy = false } }\ntools = { t = { proxy = \"http://p:1\" } }",
        ));
        assert_eq!(shown["network"]["proxy"], PLACEHOLDER);
        assert_eq!(shown["providers"]["a"]["proxy"], PLACEHOLDER);
        assert_eq!(shown["providers"]["b"]["proxy"], false);
        assert_eq!(shown["tools"]["t"]["proxy"], "http://p:1");
    }

    const FULL: &str = "client_keys = [\"gw-secret\", \"env:GATEWAY_KEY\", { name = \"app\", key = \"named-secret\" }]\nproviders = { a = { vendor = \"openai\", key = \"sk-live-1\" }, b = { formats = \"openai-chat\", base_url = \"http://b.test\", auth = { type = \"bearer\", key = \"sk-live-2\" } }, c = { vendor = \"groq\", key = \"env:GROQ_API_KEY\" } }\ntools = { m = { command = \"npx\", args = [\"-y\", \"server\", \"--token\", \"tok-1\"], env = { TOKEN = \"tok-2\", FROM_ENV = \"env:X\" } }, h = { url = \"https://h.test/mcp/\", auth = { bearer = \"tok-3\" } }, t = { url = \"https://t.test/mcp/abcdef0123456789abcdef0123\" }, q = { url = \"https://q.test/mcp?token=zzz\", headers = { x-k = \"tok-4\", x-from = \"env:Y\" } }, o = { url = \"https://o.test/mcp\", auth = { oauth = { token_url = \"https://o.test/token\", client_id = \"cid\", client_secret = \"oauth-secret\", refresh_token = \"oauth-refresh\" } } }, f = { url = \"https://f.test/mcp\", auth = \"forward\" }, s = { vendor = \"linkup\", key = \"lk-secret\" }, e = { vendor = \"linkup\", key = \"env:LK\" } }\nplugins = { commandcode_quota = { providers = [{ name = \"account\", key = \"quota-fixture-secret\" }] } }\n";

    #[test]
    fn redacts_every_kind_of_secret_and_keeps_references_and_the_rest() {
        let shown = toml::to_string_pretty(&redact(text(FULL))).expect("TOML");
        for secret in [
            "gw-secret",
            "sk-live-1",
            "sk-live-2",
            "tok-1",
            "tok-2",
            "tok-3",
            "tok-4",
            "zzz",
            "abcdef0123",
            "named-secret",
            "oauth-secret",
            "oauth-refresh",
            "lk-secret",
            "quota-fixture-secret",
        ] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        for kept in [
            "env:GATEWAY_KEY",
            "env:GROQ_API_KEY",
            "env:X",
            "env:Y",
            "auth = \"forward\"",
            "https://h.test/mcp/",
            "npx",
            "http://b.test",
            "name = \"app\"",
            "client_id = \"cid\"\n",
            "https://o.test/token",
            "env:LK",
        ] {
            assert!(shown.contains(kept), "{kept} missing in {shown}");
        }
        assert_eq!(shown.matches(PLACEHOLDER).count(), 17, "{shown}");
        assert!(shown.contains("name = \"key-1\""), "{shown}");
    }
}
