//! AWS Signature Version 4 signing for Bedrock and other AWS upstreams.
//!
//! The signer resolves static credentials from `env:`, `file:`, or `$VAR`
//! references and returns only the headers that are added to an already
//! configured request. Secrets never appear in log messages or errors; a
//! failure may name the environment variable or file path only.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use http::{HeaderName, HeaderValue, header};
use reqwest::Url;
use sha2::{Digest, Sha256};

use octra_llm_core::GatewayError;

use crate::config::SigV4Config;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const CONTENT_TYPE_VALUE: &str = "application/json";

/// Signs a request with the static credentials referenced by `config`.
///
/// The returned headers are `authorization`, `x-amz-date`,
/// `x-amz-content-sha256`, and, when configured, `x-amz-security-token`.
/// The caller must not add a second authentication header. `host` is not
/// returned because the HTTP client supplies it.
pub fn sign_request(
    config: &SigV4Config,
    method: &str,
    url: &Url,
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<Vec<(HeaderName, HeaderValue)>, GatewayError> {
    let access_key = resolve_reference("access_key_ref", &config.access_key_ref)?;
    let secret_key = resolve_reference("secret_key_ref", &config.secret_key_ref)?;
    let session_token = match &config.session_token_ref {
        Some(reference) => Some(resolve_reference("session_token_ref", reference)?),
        None => None,
    };

    sign_with_credentials(
        config,
        method,
        url,
        body,
        now,
        &access_key,
        &secret_key,
        session_token.as_deref(),
    )
}

/// Deterministic signing core. Tests pass a fixed `now`; production callers
/// use [`sign_request`] with `Utc::now()`.
#[allow(clippy::too_many_arguments)]
fn sign_with_credentials(
    config: &SigV4Config,
    method: &str,
    url: &Url,
    body: &[u8],
    now: DateTime<Utc>,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
) -> Result<Vec<(HeaderName, HeaderValue)>, GatewayError> {
    let payload_hash = hex::encode(Sha256::digest(body));
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let scope = format!("{date}/{}/{}/aws4_request", config.region, config.service);

    let host = host_header(url)?;
    let canonical_uri = url.path().to_string();
    let canonical_query = canonical_query(url);

    // Canonical headers must be sorted by lowercase header name. The fixed
    // set here is already sorted, and the optional security token is last.
    let mut headers: Vec<(&str, &str)> = vec![
        ("content-type", CONTENT_TYPE_VALUE),
        ("host", host.as_str()),
        ("x-amz-content-sha256", payload_hash.as_str()),
        ("x-amz-date", amz_date.as_str()),
    ];
    if let Some(token) = session_token {
        headers.push(("x-amz-security-token", token));
    }
    headers.sort_by(|left, right| left.0.cmp(right.0));

    let mut canonical_headers = String::new();
    let mut signed_header_names = Vec::with_capacity(headers.len());
    for (name, value) in &headers {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value);
        canonical_headers.push('\n');
        signed_header_names.push(*name);
    }
    let signed_headers = signed_header_names.join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash,
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );

    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, config.region.as_bytes());
    let k_service = hmac_sha256(&k_region, config.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let authorization = format!(
        "{ALGORITHM} Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    let mut output = Vec::with_capacity(4);
    let mut authorization_value = HeaderValue::from_str(&authorization).map_err(|_| {
        GatewayError::config(
            "sigv4 resolved configuration produced an authorization header with invalid characters",
        )
    })?;
    authorization_value.set_sensitive(true);
    output.push((header::AUTHORIZATION, authorization_value));
    output.push((
        HeaderName::from_static("x-amz-date"),
        sensitive_header("x-amz-date", &amz_date)?,
    ));
    output.push((
        HeaderName::from_static("x-amz-content-sha256"),
        sensitive_header("x-amz-content-sha256", &payload_hash)?,
    ));
    if let Some(token) = session_token {
        output.push((
            HeaderName::from_static("x-amz-security-token"),
            sensitive_header("x-amz-security-token", token)?,
        ));
    }
    Ok(output)
}

fn resolve_reference(label: &str, reference: &str) -> Result<String, GatewayError> {
    crate::upstream::resolve_secret_ref(reference).map_err(|message| {
        GatewayError::config(format!("sigv4 {label}: {message}")).with_code("secret_unavailable")
    })
}

fn sensitive_header(name: &str, value: &str) -> Result<HeaderValue, GatewayError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        GatewayError::config(format!(
            "sigv4 {name} cannot be encoded as an HTTP header value"
        ))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn host_header(url: &Url) -> Result<String, GatewayError> {
    let host = url
        .host_str()
        .ok_or_else(|| GatewayError::config("sigv4 cannot sign a URL without a host"))?;
    match url.port() {
        Some(port) => Ok(format!("{host}:{port}")),
        None => Ok(host.to_string()),
    }
}

/// Builds the AWS canonical query string. Pair names and values are
/// re-encoded with the SigV4 unreserved set and sorted by name, then value.
fn canonical_query(url: &Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (aws_encode(&name), aws_encode(&value)))
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct SecretFiles {
        dir: std::path::PathBuf,
        access_key_ref: String,
        secret_key_ref: String,
        session_token_ref: Option<String>,
    }

    impl SecretFiles {
        fn new(session_token: Option<&str>) -> Self {
            let dir = std::env::temp_dir().join(format!("octra-sigv4-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let access = dir.join("access_key");
            let secret = dir.join("secret_key");
            std::fs::write(&access, "AKIDEXAMPLE\n").unwrap();
            std::fs::write(&secret, "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY\n").unwrap();
            let session_token_ref = session_token.map(|token| {
                let path = dir.join("session_token");
                std::fs::write(&path, format!("{token}\n")).unwrap();
                format!("file:{}", path.display())
            });
            Self {
                dir,
                access_key_ref: format!("file:{}", access.display()),
                secret_key_ref: format!("file:{}", secret.display()),
                session_token_ref,
            }
        }

        fn config(&self, region: &str, service: &str) -> SigV4Config {
            SigV4Config {
                region: region.to_string(),
                service: service.to_string(),
                access_key_ref: self.access_key_ref.clone(),
                secret_key_ref: self.secret_key_ref.clone(),
                session_token_ref: self.session_token_ref.clone(),
            }
        }
    }

    impl Drop for SecretFiles {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    fn bedrock_url() -> Url {
        Url::parse(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse",
        )
        .unwrap()
    }

    fn body() -> &'static [u8] {
        br#"{"messages":[{"role":"user","content":[{"text":"hello"}]}]}"#
    }

    fn authorization(headers: &[(HeaderName, HeaderValue)]) -> String {
        headers
            .iter()
            .find(|(name, _)| name == header::AUTHORIZATION)
            .map(|(_, value)| value.to_str().unwrap().to_string())
            .expect("authorization header")
    }

    fn header<'a>(headers: &'a [(HeaderName, HeaderValue)], name: &str) -> Option<&'a HeaderValue> {
        headers
            .iter()
            .find(|(header, _)| header.as_str() == name)
            .map(|(_, value)| value)
    }

    #[test]
    fn deterministic_signature_for_fixed_request() {
        let secrets = SecretFiles::new(None);
        let signed = sign_request(
            &secrets.config("us-east-1", "bedrock"),
            "POST",
            &bedrock_url(),
            body(),
            fixed_time(),
        )
        .unwrap();

        assert_eq!(
            authorization(&signed),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260102/us-east-1/bedrock/aws4_request, \
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, \
             Signature=71d1b3df277efb6475cf4f94adfce9d2fae3dad6294e0fcd61816cd94d5c0dca"
        );
        assert_eq!(
            header(&signed, "x-amz-date").unwrap().to_str().unwrap(),
            "20260102T030405Z"
        );
        assert_eq!(
            header(&signed, "x-amz-content-sha256")
                .unwrap()
                .to_str()
                .unwrap(),
            hex::encode(Sha256::digest(body()))
        );
        assert!(header(&signed, "authorization").unwrap().is_sensitive());
    }

    #[test]
    fn signature_changes_with_body_time_region_and_service() {
        let secrets = SecretFiles::new(None);
        let config = secrets.config("us-east-1", "bedrock");
        let baseline = authorization(
            &sign_request(&config, "POST", &bedrock_url(), body(), fixed_time()).unwrap(),
        );

        let changed_body = authorization(
            &sign_request(
                &config,
                "POST",
                &bedrock_url(),
                br#"{"messages":[]}"#,
                fixed_time(),
            )
            .unwrap(),
        );
        assert_ne!(baseline, changed_body);

        let later = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 6).unwrap();
        let changed_time =
            authorization(&sign_request(&config, "POST", &bedrock_url(), body(), later).unwrap());
        assert_ne!(baseline, changed_time);

        let changed_region = authorization(
            &sign_request(
                &secrets.config("us-west-2", "bedrock"),
                "POST",
                &bedrock_url(),
                body(),
                fixed_time(),
            )
            .unwrap(),
        );
        assert_ne!(baseline, changed_region);

        let changed_service = authorization(
            &sign_request(
                &secrets.config("us-east-1", "bedrock-runtime"),
                "POST",
                &bedrock_url(),
                body(),
                fixed_time(),
            )
            .unwrap(),
        );
        assert_ne!(baseline, changed_service);
    }

    #[test]
    fn session_token_is_signed_when_present() {
        let secrets = SecretFiles::new(Some("session-token-value"));
        let signed = sign_request(
            &secrets.config("us-east-1", "bedrock"),
            "POST",
            &bedrock_url(),
            body(),
            fixed_time(),
        )
        .unwrap();

        assert_eq!(
            header(&signed, "x-amz-security-token")
                .unwrap()
                .to_str()
                .unwrap(),
            "session-token-value"
        );
        let with_token = authorization(&signed);
        assert!(with_token.contains("x-amz-security-token"));
        assert!(with_token.contains(
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
        ));

        let without_token = sign_request(
            &SecretFiles::new(None).config("us-east-1", "bedrock"),
            "POST",
            &bedrock_url(),
            body(),
            fixed_time(),
        )
        .unwrap();
        assert_ne!(with_token, authorization(&without_token));
    }

    #[test]
    #[allow(unsafe_code)]
    fn env_and_file_references_resolve() {
        let secrets = SecretFiles::new(None);
        let file_signed = sign_request(
            &secrets.config("us-east-1", "bedrock"),
            "POST",
            &bedrock_url(),
            body(),
            fixed_time(),
        )
        .unwrap();
        assert!(authorization(&file_signed).contains("Credential=AKIDEXAMPLE/"));

        let env_name = format!("OCTRA_TEST_SIGV4_SECRET_{}", uuid::Uuid::new_v4().simple());
        // SAFETY: test-only environment mutation with a unique variable name.
        unsafe {
            std::env::set_var(&env_name, "env-secret-key\n");
        }
        let env_config = SigV4Config {
            region: "us-east-1".to_string(),
            service: "bedrock".to_string(),
            access_key_ref: secrets.access_key_ref.clone(),
            secret_key_ref: format!("env:{env_name}"),
            session_token_ref: None,
        };
        let env_signed =
            sign_request(&env_config, "POST", &bedrock_url(), body(), fixed_time()).unwrap();
        assert!(!authorization(&env_signed).is_empty());
        // SAFETY: removing the unique variable set above.
        unsafe {
            std::env::remove_var(&env_name);
        }
    }

    #[test]
    fn missing_references_report_names_not_values() {
        let secrets = SecretFiles::new(None);
        let missing_env = "OCTRA_TEST_SIGV4_MISSING_ACCESS_KEY";
        let config = SigV4Config {
            region: "us-east-1".to_string(),
            service: "bedrock".to_string(),
            access_key_ref: format!("env:{missing_env}"),
            secret_key_ref: secrets.secret_key_ref.clone(),
            session_token_ref: None,
        };
        let error =
            sign_request(&config, "POST", &bedrock_url(), body(), fixed_time()).unwrap_err();
        assert_eq!(error.kind, octra_llm_core::ErrorKind::Config);
        assert!(error.message.contains(missing_env), "{}", error.message);
        assert!(
            !error
                .message
                .contains("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
        );

        let dir = std::env::temp_dir().join(format!("octra-sigv4-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret_key");
        std::fs::write(&path, "never-echo-this-value\n").unwrap();
        std::fs::remove_file(&path).unwrap();
        let config = SigV4Config {
            region: "us-east-1".to_string(),
            service: "bedrock".to_string(),
            access_key_ref: secrets.access_key_ref.clone(),
            secret_key_ref: format!("file:{}", path.display()),
            session_token_ref: None,
        };
        let error =
            sign_request(&config, "POST", &bedrock_url(), body(), fixed_time()).unwrap_err();
        assert!(error.message.contains("secret_key"), "{}", error.message);
        assert!(!error.message.contains("never-echo-this-value"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_strings_are_sorted_and_aws_encoded() {
        let url = Url::parse(
            "https://example.amazonaws.com/path?z=last&a=hello%20world&a=first&tilde=%7E&empty",
        )
        .unwrap();
        assert_eq!(
            canonical_query(&url),
            "a=first&a=hello%20world&empty=&tilde=~&z=last"
        );

        let secrets = SecretFiles::new(None);
        let config = secrets.config("us-east-1", "bedrock");
        let first = Url::parse("https://example.amazonaws.com/path?b=2&a=1").unwrap();
        let second = Url::parse("https://example.amazonaws.com/path?a=1&b=2").unwrap();
        assert_eq!(
            authorization(&sign_request(&config, "GET", &first, b"", fixed_time()).unwrap()),
            authorization(&sign_request(&config, "GET", &second, b"", fixed_time()).unwrap())
        );
    }
}
