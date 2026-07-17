//! OAuth 2.0 authorization-code + PKCE for per-user connectors (blueprint §15).
//!
//! The consent step is a **human copy-paste**, not a headless action (§15): Skald
//! builds a consent URL, the user approves it in a browser, and the provider lands
//! the `code` on a static page (`redirect_uri`, e.g. `oauth/show.html`) that shows
//! it for copying. Skald then exchanges the code for a refresh token. PKCE means an
//! intercepted code is useless without the verifier, which never leaves this
//! process — so the copy-paste page can be a plain static file with no backend.
//!
//! The obtained refresh token is delivered to the connector's server per its
//! manifest `auth.deliver` spec; only `env` delivery is wired (the credential is
//! injected as an environment variable at `docker exec` time — nothing on disk).

use anyhow::{Context, Result, bail};
use base64::Engine;
use rand::Rng as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::db::oauth_providers::OauthProviderRow;

/// How Skald delivers the obtained credential to the connector's server process,
/// mirrored from the manifest's `auth.deliver` (§15).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeliverSpec {
    /// `env` | `file`. Only `env` is implemented; a `file` target is rejected at
    /// activation with a clear message rather than silently half-working.
    #[serde(rename = "as")]
    pub as_:    String,
    /// The serialization Skald must produce (`google_authorized_user` | `refresh_token`).
    #[serde(default)]
    pub format: Option<String>,
    /// `as=env`: the environment variable the credential is injected into.
    #[serde(default)]
    pub env:    Option<String>,
    /// `as=file`: the target path (unused while file delivery is unimplemented).
    #[serde(default)]
    pub path:   Option<String>,
}

const URL_SAFE: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A high-entropy PKCE verifier and its S256 challenge (RFC 7636).
pub struct Pkce {
    pub verifier:  String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE.encode(bytes); // 43-char base64url, within the RFC range
    let challenge = URL_SAFE.encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

/// An opaque, URL-safe `state` value: CSRF guard and the key of the pending flow.
pub fn random_state() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE.encode(bytes)
}

/// Builds the authorization-endpoint URL the user opens to consent. Merges the
/// provider's `extra_params` (Google needs `access_type=offline` + `prompt=consent`
/// to return a refresh token) after the standard params.
pub fn build_consent_url(
    provider:  &OauthProviderRow,
    scopes:    &[String],
    state:     &str,
    challenge: &str,
) -> Result<String> {
    let scope = scopes.join(" ");
    let mut params: Vec<(String, String)> = vec![
        ("client_id".into(),             provider.client_id.clone()),
        ("redirect_uri".into(),          provider.redirect_uri.clone()),
        ("response_type".into(),         "code".into()),
        ("scope".into(),                 scope),
        ("state".into(),                 state.into()),
        ("code_challenge".into(),        challenge.into()),
        ("code_challenge_method".into(), "S256".into()),
    ];
    for (k, v) in provider.extra() {
        params.push((k, v));
    }
    let url = reqwest::Url::parse_with_params(&provider.auth_url, &params)
        .with_context(|| format!("invalid authorization endpoint `{}`", provider.auth_url))?;
    Ok(url.to_string())
}

/// The token endpoint's response. Google returns `refresh_token` only on the first
/// consent for a client, or when `prompt=consent` forces re-issue — hence the
/// provider's `extra_params`.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    #[serde(default)] pub access_token:      Option<String>,
    #[serde(default)] pub refresh_token:     Option<String>,
    #[serde(default)] pub expires_in:        Option<i64>,
    #[serde(default)] pub scope:             Option<String>,
    #[serde(default)] pub error:             Option<String>,
    #[serde(default)] pub error_description: Option<String>,
}

/// Exchanges an authorization `code` (+ PKCE `verifier`) for tokens at the
/// provider's token endpoint.
pub async fn exchange_code(
    provider: &OauthProviderRow,
    code:     &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let params = [
        ("grant_type",    "authorization_code"),
        ("code",          code),
        ("client_id",     provider.client_id.as_str()),
        ("client_secret", provider.client_secret.as_str()),
        ("redirect_uri",  provider.redirect_uri.as_str()),
        ("code_verifier", verifier),
    ];
    // `RequestBuilder::form` needs reqwest's `urlencoded` feature, which this build
    // doesn't enable — so encode the body ourselves. Parsing a throwaway URL with
    // these params yields exactly the `application/x-www-form-urlencoded` string.
    let body = reqwest::Url::parse_with_params("http://form.local/", &params)
        .ok()
        .and_then(|u| u.query().map(str::to_owned))
        .unwrap_or_default();
    let resp = reqwest::Client::new()
        .post(&provider.token_url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("token endpoint request failed")?;
    let status = resp.status();
    let body: TokenResponse = resp
        .json()
        .await
        .context("token endpoint returned a non-JSON body")?;
    if let Some(err) = &body.error {
        let detail = body.error_description.as_deref()
            .map(|d| format!(" — {d}")).unwrap_or_default();
        bail!("token exchange failed: {err}{detail}");
    }
    if !status.is_success() {
        bail!("token exchange failed with HTTP {status}");
    }
    Ok(body)
}

/// Serializes a refresh token into the shape the connector's server reads, per
/// `deliver.format`. `google_authorized_user` is the JSON that
/// `google.oauth2.credentials.Credentials.from_authorized_user_info` accepts — the
/// server refreshes access tokens from it on its own.
pub fn assemble_credential(
    format:        &str,
    provider:      &OauthProviderRow,
    refresh_token: &str,
) -> Result<String> {
    match format {
        "google_authorized_user" => Ok(serde_json::json!({
            "type":          "authorized_user",
            "client_id":     provider.client_id,
            "client_secret": provider.client_secret,
            "refresh_token": refresh_token,
            "token_uri":     provider.token_url,
        }).to_string()),
        "refresh_token" => Ok(refresh_token.to_string()),
        other => bail!("unsupported deliver.format `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = generate_pkce();
        let expect = URL_SAFE.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        assert!(!p.verifier.contains(['+', '/', '=']), "verifier must be url-safe, unpadded");
    }

    #[test]
    fn authorized_user_credential_has_googles_fields() {
        let provider = OauthProviderRow {
            name: "google".into(), display_name: "Google".into(),
            auth_url: "https://a".into(), token_url: "https://t".into(),
            client_id: "cid".into(), client_secret: "csec".into(),
            redirect_uri: "https://r".into(), extra_params: None,
            created_at: String::new(), updated_at: String::new(),
        };
        let cred = assemble_credential("google_authorized_user", &provider, "rt-123").unwrap();
        let v: serde_json::Value = serde_json::from_str(&cred).unwrap();
        assert_eq!(v["type"], "authorized_user");
        assert_eq!(v["client_id"], "cid");
        assert_eq!(v["refresh_token"], "rt-123");
        assert_eq!(v["token_uri"], "https://t");
    }
}
