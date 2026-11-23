use super::{header, ApiError, AppState, HeaderMap, HeaderValue, Json, State, StatusCode};
use ring::hmac;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const SESSION_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserAuthConfig {
    access_key_sha256: Option<String>,
    trusted_proxy: Option<TrustedProxyConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedProxyConfig {
    header: String,
    value: String,
}

#[derive(Clone, Default)]
pub(super) struct BrowserAuth {
    access_key_digest: Option<String>,
    trusted_proxy: Option<(header::HeaderName, HeaderValue)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Methods {
    access_key: bool,
    trusted_proxy: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Credentials {
    access_key: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Session {
    launch_capability: String,
}

impl BrowserAuth {
    pub(super) fn load(path: &str, origin: &str) -> Result<Self, String> {
        if path.is_empty() {
            return Ok(Self::default());
        }
        if !origin.starts_with("https://") {
            return Err("browser_auth_file requires an HTTPS browser_origin".into());
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take(16_385).read_to_end(&mut bytes))
            .map_err(|error| format!("could not read browser_auth_file: {error}"))?;
        if bytes.len() > 16_384 {
            return Err("browser_auth_file exceeds 16 KiB".into());
        }
        let config: BrowserAuthConfig =
            serde_json::from_slice(&bytes).map_err(|_| "invalid browser_auth_file JSON")?;
        if config
            .access_key_sha256
            .as_deref()
            .is_some_and(|digest| !is_key(digest))
        {
            return Err("accessKeySha256 must be 64 lowercase hexadecimal characters".into());
        }
        let trusted_proxy = config
            .trusted_proxy
            .map(|proxy| {
                let name = header::HeaderName::from_bytes(proxy.header.as_bytes())
                    .map_err(|_| "trustedProxy.header is not a valid header name")?;
                if matches!(
                    name.as_str(),
                    "host" | "origin" | "authorization" | "cookie"
                ) || name.as_str().starts_with("sec-")
                {
                    return Err("trustedProxy.header must be a dedicated identity header");
                }
                let value = HeaderValue::from_str(&proxy.value)
                    .map_err(|_| "trustedProxy.value is not a valid header value")?;
                if value.is_empty() {
                    return Err("trustedProxy.value must not be empty");
                }
                Ok((name, value))
            })
            .transpose()?;
        Ok(Self {
            access_key_digest: config.access_key_sha256,
            trusted_proxy,
        })
    }

    fn accepts_proxy(&self, headers: &HeaderMap) -> bool {
        let Some((name, expected)) = &self.trusted_proxy else {
            return false;
        };
        let mut values = headers.get_all(name).iter();
        values.next() == Some(expected) && values.next().is_none()
    }
}

pub(super) async fn methods(State(state): State<AppState>) -> Json<Methods> {
    Json(Methods {
        access_key: state.browser_auth.access_key_digest.is_some(),
        trusted_proxy: state.browser_auth.trusted_proxy.is_some(),
    })
}

pub(super) async fn sign_in(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(credentials): Json<Credentials>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let unauthorized = || {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "sign_in_required",
            "Sign in to open this notebook.",
        )
    };
    if headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        != Some(&state.origin)
    {
        return Err(unauthorized());
    }

    let mut output_headers = HeaderMap::new();
    if let Some(access_key) = credentials.access_key {
        let digest = state
            .browser_auth
            .access_key_digest
            .as_deref()
            .ok_or_else(unauthorized)?;
        if !is_key(&access_key)
            || !bool::from(
                format!("{:x}", Sha256::digest(access_key))
                    .as_bytes()
                    .ct_eq(digest.as_bytes()),
            )
        {
            return Err(unauthorized());
        }
        set_cookie(&mut output_headers, &state.origin, digest);
    } else if !state.browser_auth.accepts_proxy(&headers)
        && !state
            .browser_auth
            .access_key_digest
            .as_deref()
            .is_some_and(|digest| accepts_cookie(&headers, &state.origin, digest))
    {
        return Err(unauthorized());
    }
    Ok((
        output_headers,
        Json(Session {
            launch_capability: state.token,
        }),
    ))
}

fn is_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cookie_name(origin: &str) -> String {
    let hash = format!("{:x}", Sha256::digest(origin));
    format!("__Host-sqlnb-{}", &hash[..16])
}

fn signature(origin: &str, digest: &str, expiry: u64) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, digest.as_bytes());
    let message = format!("sqlnb-session-v1\n{origin}\n{expiry}");
    hmac::sign(&key, message.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn set_cookie(headers: &mut HeaderMap, origin: &str, digest: &str) {
    let expiry = unix_seconds() + SESSION_SECONDS;
    let cookie = format!(
        "{}={expiry}.{}; Max-Age={SESSION_SECONDS}; Path=/; Secure; HttpOnly; SameSite=Strict",
        cookie_name(origin),
        signature(origin, digest, expiry),
    );
    headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
}

fn accepts_cookie(headers: &HeaderMap, origin: &str, digest: &str) -> bool {
    let name = cookie_name(origin);
    let value = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value));
    let Some((expiry, mac)) = value.and_then(|value| value.split_once('.')) else {
        return false;
    };
    let Ok(expiry) = expiry.parse::<u64>() else {
        return false;
    };
    expiry > unix_seconds()
        && is_key(mac)
        && bool::from(
            signature(origin, digest, expiry)
                .as_bytes()
                .ct_eq(mac.as_bytes()),
        )
}
