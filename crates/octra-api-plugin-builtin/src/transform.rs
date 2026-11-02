//! Shared helpers for Kong-style transformer plugins.

use http::{HeaderMap, HeaderName, HeaderValue, header};
use serde_json::Value;

/// Splits a Kong transformation entry. The first colon separates the name from
/// the value; extra leading colons are ignored.
pub(crate) fn split_pair(entry: &str) -> (&str, Option<&str>) {
    match entry.split_once(':') {
        Some((name, value)) => (name, Some(value.trim_start_matches(':'))),
        None => (entry, None),
    }
}

/// Applies remove, rename, replace, add, and append operations in that order.
pub(crate) fn apply_header_ops(
    headers: &mut HeaderMap,
    remove: &[String],
    rename: &[String],
    replace: &[String],
    add: &[String],
    append: &[String],
    skip_host: bool,
) {
    let host = HeaderName::from_static("host");

    for entry in remove {
        let (name, _) = split_pair(entry);
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
            && !(skip_host && name == host)
        {
            headers.remove(name);
        }
    }

    for entry in rename {
        let (old, new) = split_pair(entry);
        let Some(new) = new else { continue };
        let (Ok(old), Ok(new)) = (
            HeaderName::from_bytes(old.as_bytes()),
            HeaderName::from_bytes(new.as_bytes()),
        ) else {
            continue;
        };
        if skip_host && (old == host || new == host) {
            continue;
        }
        if let Some(value) = headers.remove(&old) {
            headers.insert(new, value);
        }
    }

    for entry in replace {
        let (name, value) = split_pair(entry);
        let Some(value) = value else { continue };
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        if skip_host && name == host {
            continue;
        }
        if headers.contains_key(&name) {
            headers.insert(name, value);
        }
    }

    for entry in add {
        let (name, value) = split_pair(entry);
        let Some(value) = value else { continue };
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        if skip_host && name == host {
            continue;
        }
        if !headers.contains_key(&name) {
            headers.insert(name, value);
        }
    }

    for entry in append {
        let (name, value) = split_pair(entry);
        let Some(value) = value else { continue };
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        if skip_host && name == host {
            continue;
        }
        headers.append(name, value);
    }
}

pub(crate) fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("application/json")
                || value
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().ends_with("+json"))
        })
}

pub(crate) fn is_form_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .to_ascii_lowercase()
                .contains("application/x-www-form-urlencoded")
        })
}

pub(crate) fn parse_form(raw: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

pub(crate) fn encode_form(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

pub(crate) fn form_remove(pairs: &mut Vec<(String, String)>, name: &str) {
    pairs.retain(|(key, _)| key != name);
}

pub(crate) fn form_rename(pairs: &mut Vec<(String, String)>, old: &str, new: &str) {
    if old == new {
        return;
    }
    let values: Vec<String> = pairs
        .iter()
        .filter(|(key, _)| key == old)
        .map(|(_, value)| value.clone())
        .collect();
    if values.is_empty() {
        return;
    }
    pairs.retain(|(key, _)| key != old && key != new);
    pairs.extend(values.into_iter().map(|value| (new.to_string(), value)));
}

pub(crate) fn form_replace(pairs: &mut Vec<(String, String)>, name: &str, value: &str) {
    if !pairs.iter().any(|(key, _)| key == name) {
        return;
    }
    pairs.retain(|(key, _)| key != name);
    pairs.push((name.to_string(), value.to_string()));
}

pub(crate) fn form_add(pairs: &mut Vec<(String, String)>, name: &str, value: &str) {
    if !pairs.iter().any(|(key, _)| key == name) {
        pairs.push((name.to_string(), value.to_string()));
    }
}

pub(crate) fn form_append(pairs: &mut Vec<(String, String)>, name: &str, value: &str) {
    pairs.push((name.to_string(), value.to_string()));
}

pub(crate) fn json_append(existing: Option<&Value>, value: Value) -> Value {
    match existing {
        Some(Value::Array(items)) => {
            let mut items = items.clone();
            items.push(value);
            Value::Array(items)
        }
        Some(other) => Value::Array(vec![other.clone(), value]),
        None => Value::Array(vec![value]),
    }
}
