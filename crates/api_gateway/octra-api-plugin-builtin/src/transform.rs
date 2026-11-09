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

pub(crate) fn is_multipart_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("multipart/form-data"))
}

pub(crate) struct MultipartPart {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MultipartPart {
    fn simple(name: &str, value: &str) -> Self {
        Self {
            headers: vec![(
                "Content-Disposition".to_string(),
                format!("form-data; name=\"{name}\""),
            )],
            body: value.as_bytes().to_vec(),
        }
    }

    fn name(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-disposition"))
            .and_then(|(_, value)| name_param(value))
    }

    fn set_name(&mut self, new_name: &str) {
        for (name, value) in &mut self.headers {
            if name.eq_ignore_ascii_case("content-disposition") {
                *value = set_name_param(value, new_name);
            }
        }
    }
}

pub(crate) struct MultipartBody {
    boundary: String,
    parts: Vec<MultipartPart>,
}

impl MultipartBody {
    pub(crate) fn parse(content_type: &str, body: &[u8]) -> Option<Self> {
        let boundary = multipart_boundary(content_type)?;
        let delimiter = format!("--{boundary}").into_bytes();
        let mut parts = Vec::new();
        let mut position = 0usize;

        while let Some(index) = find_bytes(&body[position..], &delimiter) {
            let mut start = position + index + delimiter.len();
            if body.get(start..start + 2) == Some(b"--") {
                break;
            }
            if body.get(start..start + 2) == Some(b"\r\n") {
                start += 2;
            }
            let Some(next) = find_bytes(&body[start..], &delimiter) else {
                break;
            };
            let mut end = start + next;
            if body.get(end.saturating_sub(2)..end) == Some(b"\r\n") {
                end -= 2;
            }
            if let Some(part) = parse_part(&body[start..end]) {
                parts.push(part);
            }
            position = start + next;
        }

        Some(Self { boundary, parts })
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut output = Vec::new();
        for part in &self.parts {
            output.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
            for (name, value) in &part.headers {
                output.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
            }
            output.extend_from_slice(b"\r\n");
            output.extend_from_slice(&part.body);
            output.extend_from_slice(b"\r\n");
        }
        output.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        output
    }

    pub(crate) fn value(&self, name: &str) -> Option<String> {
        self.parts
            .iter()
            .find(|part| part.name() == Some(name))
            .and_then(|part| String::from_utf8(part.body.clone()).ok())
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.parts
            .retain(|part| part.name().is_none_or(|part_name| part_name != name));
    }

    pub(crate) fn rename(&mut self, old: &str, new: &str) {
        for part in &mut self.parts {
            if part.name() == Some(old) {
                part.set_name(new);
            }
        }
    }

    pub(crate) fn replace(&mut self, name: &str, value: &str) {
        if !self.parts.iter().any(|part| part.name() == Some(name)) {
            return;
        }
        self.parts
            .retain(|part| part.name().is_none_or(|part_name| part_name != name));
        self.parts.push(MultipartPart::simple(name, value));
    }

    pub(crate) fn add(&mut self, name: &str, value: &str) {
        if !self.parts.iter().any(|part| part.name() == Some(name)) {
            self.parts.push(MultipartPart::simple(name, value));
        }
    }
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    for segment in content_type.split(';') {
        let segment = segment.trim();
        let Some((name, value)) = segment.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("boundary") {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn name_param(value: &str) -> Option<&str> {
    for segment in value.split(';') {
        let segment = segment.trim();
        if let Some(rest) = segment.strip_prefix("name=") {
            return Some(rest.trim_matches('"'));
        }
    }
    None
}

fn set_name_param(value: &str, new_name: &str) -> String {
    let mut replaced = false;
    let mut segments = Vec::new();
    for segment in value.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if segment.starts_with("name=") {
            segments.push(format!("name=\"{new_name}\""));
            replaced = true;
        } else {
            segments.push(segment.to_string());
        }
    }
    if !replaced {
        segments.push(format!("name=\"{new_name}\""));
    }
    segments.join("; ")
}

fn parse_part(raw: &[u8]) -> Option<MultipartPart> {
    let split = find_bytes(raw, b"\r\n\r\n")?;
    let headers = String::from_utf8_lossy(&raw[..split]);
    let body = raw[split + 4..].to_vec();
    let headers = headers
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    Some(MultipartPart { headers, body })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MULTIPART_TYPE: &str = "multipart/form-data; boundary=b";
    const MULTIPART: &[u8] = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--b\r\nContent-Disposition: form-data; name=\"b\"\r\n\r\n2\r\n--b--\r\n";

    #[test]
    fn splits_pairs_on_the_first_colon() {
        assert_eq!(split_pair("a:b"), ("a", Some("b")));
        assert_eq!(split_pair("a"), ("a", None));
        assert_eq!(split_pair("a::b"), ("a", Some("b")));
        assert_eq!(split_pair("a:"), ("a", Some("")));
        assert_eq!(split_pair(""), ("", None));
    }

    #[test]
    fn applies_header_operations_in_order() {
        let mut headers = HeaderMap::new();
        headers.insert("x-remove", HeaderValue::from_static("1"));
        headers.insert("x-rename", HeaderValue::from_static("rename"));
        headers.insert("x-replace", HeaderValue::from_static("old"));
        headers.insert("x-add", HeaderValue::from_static("keep"));
        headers.insert("x-append", HeaderValue::from_static("first"));

        apply_header_ops(
            &mut headers,
            &["x-remove".to_string()],
            &["x-rename:x-new".to_string()],
            &["x-replace:new".to_string(), "x-missing:new".to_string()],
            &["x-add:new".to_string(), "x-missing-add:value".to_string()],
            &["x-append:second".to_string()],
            false,
        );

        assert!(!headers.contains_key("x-remove"));
        assert!(!headers.contains_key("x-rename"));
        assert_eq!(
            headers.get("x-new").and_then(|value| value.to_str().ok()),
            Some("rename")
        );
        assert_eq!(
            headers
                .get("x-replace")
                .and_then(|value| value.to_str().ok()),
            Some("new")
        );
        assert!(!headers.contains_key("x-missing"));
        assert_eq!(
            headers.get("x-add").and_then(|value| value.to_str().ok()),
            Some("keep")
        );
        assert_eq!(
            headers
                .get("x-missing-add")
                .and_then(|value| value.to_str().ok()),
            Some("value")
        );
        let appended: Vec<_> = headers
            .get_all("x-append")
            .iter()
            .map(|value| value.to_str().expect("ascii"))
            .collect();
        assert_eq!(appended, ["first", "second"]);
    }

    #[test]
    fn skip_host_protects_the_host_header() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("original"));
        headers.insert("x-rename", HeaderValue::from_static("rename"));

        apply_header_ops(
            &mut headers,
            &["host".to_string()],
            &["x-rename:host".to_string()],
            &["host:new".to_string()],
            &["host:new".to_string()],
            &["host:new".to_string()],
            true,
        );

        assert_eq!(
            headers.get("host").and_then(|value| value.to_str().ok()),
            Some("original")
        );
        assert_eq!(
            headers
                .get("x-rename")
                .and_then(|value| value.to_str().ok()),
            Some("rename")
        );
    }

    #[test]
    fn detects_content_types_with_parameters() {
        let mut headers = HeaderMap::new();
        assert!(!is_json_content_type(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(is_json_content_type(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.api+json"),
        );
        assert!(is_json_content_type(&headers));

        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!is_json_content_type(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded; charset=utf-8"),
        );
        assert!(is_form_content_type(&headers));
        assert!(!is_multipart_content_type(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Multipart/Form-Data; boundary=b"),
        );
        assert!(is_multipart_content_type(&headers));
    }

    #[test]
    fn parses_and_serializes_multipart_bodies() {
        let mut multipart = MultipartBody::parse(MULTIPART_TYPE, MULTIPART).expect("multipart");
        assert_eq!(multipart.value("a").as_deref(), Some("1"));
        assert_eq!(multipart.value("b").as_deref(), Some("2"));
        assert_eq!(multipart.value("missing"), None);

        multipart.add("c", "3");
        multipart.add("a", "ignored");
        assert_eq!(multipart.value("a").as_deref(), Some("1"));
        assert_eq!(multipart.value("c").as_deref(), Some("3"));

        multipart.replace("a", "updated");
        assert_eq!(multipart.value("a").as_deref(), Some("updated"));
        multipart.replace("missing", "ignored");
        assert_eq!(multipart.value("missing"), None);

        multipart.rename("b", "renamed");
        assert_eq!(multipart.value("b"), None);
        assert_eq!(multipart.value("renamed").as_deref(), Some("2"));

        multipart.remove("c");
        assert_eq!(multipart.value("c"), None);

        let reparsed =
            MultipartBody::parse(MULTIPART_TYPE, &multipart.to_bytes()).expect("multipart");
        assert_eq!(reparsed.value("a").as_deref(), Some("updated"));
        assert_eq!(reparsed.value("renamed").as_deref(), Some("2"));

        assert!(MultipartBody::parse("multipart/form-data", MULTIPART).is_none());
    }

    #[test]
    fn parses_and_mutates_forms() {
        let mut pairs = parse_form("a=1&b=2&b=3&c=x%20y");
        assert_eq!(
            pairs,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
                ("b".into(), "3".into()),
                ("c".into(), "x y".into()),
            ]
        );
        assert_eq!(parse_form(&encode_form(&pairs)), pairs);

        form_remove(&mut pairs, "b");
        assert!(pairs.iter().all(|(key, _)| key != "b"));

        form_rename(&mut pairs, "a", "alpha");
        assert_eq!(
            pairs
                .iter()
                .find(|(key, _)| key == "alpha")
                .map(|(_, value)| value.as_str()),
            Some("1")
        );

        form_replace(&mut pairs, "alpha", "updated");
        assert_eq!(
            pairs
                .iter()
                .find(|(key, _)| key == "alpha")
                .map(|(_, value)| value.as_str()),
            Some("updated")
        );
        form_replace(&mut pairs, "missing", "ignored");

        form_add(&mut pairs, "alpha", "ignored");
        assert_eq!(pairs.iter().filter(|(key, _)| key == "alpha").count(), 1);
        form_add(&mut pairs, "new", "value");
        form_append(&mut pairs, "new", "second");
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key == "new")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            ["value", "second"]
        );
    }

    #[test]
    fn appends_json_values() {
        assert_eq!(json_append(None, json!(1)), json!([1]));
        assert_eq!(json_append(Some(&json!(1)), json!(2)), json!([1, 2]));
        assert_eq!(
            json_append(Some(&json!([1, 2])), json!(3)),
            json!([1, 2, 3])
        );
    }
}
