// WebDAV transport (Task 4) — Sisyphus cross-device sync.
//
// Scope: this module moves ONE `SyncSnapshot` JSON document in and out of a
// WebDAV folder. It knows nothing about merge rules, tombstones, `sync_id`
// dedup, `last_read_at` MAX or conflict resolution — all of that lives in
// `sync.rs` and is reused verbatim. The transport contract is deliberately
// tiny:
//
//     get_latest() -> RemoteObject        (404 => RemoteNotFound, not an error)
//     put_latest(bytes, condition)        (412 => PreconditionFailed)
//     ensure_dir() / options / propfind / mkcol   (setup + test-connection)
//
// Remote layout — one directory, one file, no history:
//
//     <configured WebDAV root>/
//       sync/
//         latest.iksync
//
// There is no change log, no per-device directory, no revision history, no
// remote DB and no lock server. v1 stores a single full snapshot.
//
// Concurrency: the transport is only half of the story. `sync::sync_via_webdav`
// reads the remote, merges locally and writes the merged snapshot back with an
// `If-Match` precondition built from the ETag observed on GET. A 412 means
// another device committed in between, and the caller re-fetches / re-merges /
// re-uploads (bounded retries). Servers that do not return ETags degrade to an
// unconditional PUT — see `LIMITATIONS` below.
//
// Credentials: the password is attached only to the outgoing
// `Authorization` header. It is never logged, never written to the knowledge
// DB, never embedded in an error value (every error string is passed through
// `redact_secrets`, which also strips inline `user:pass@` from URLs), and
// never returned to the UI (`webdav_config_get` blanks it).
//
// LIMITATIONS (v1, deliberate):
//   * Servers that neither return an `ETag` nor honour `If-Match` /
//     `If-None-Match` fall back to an unconditional, last-writer-wins PUT.
//     Data is still never lost *locally* (the merge happens before the
//     upload and is idempotent), but a genuinely simultaneous pair of
//     uploads could have one side's snapshot overwritten until the next
//     sync. Supporting servers are protected by the precondition retry loop.
//   * No remote history: a corrupt `latest.iksync` is left in place for the
//     user to inspect (the app never covers it with local data).

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use reqwest::header::{IF_MATCH, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};

use crate::config::WebDavConfig;

/// Boxed future returned by the async transport methods. Hand-rolled instead
/// of pulling in `async-trait`; mirrors the `BoxFuture` alias already used by
/// the chat engine.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Remote directory (relative to the configured root) that holds the
/// snapshot.
pub const REMOTE_DIR: &str = "sync";
/// Remote snapshot file name.
pub const REMOTE_FILE: &str = "latest.iksync";
/// `<REMOTE_DIR>/<REMOTE_FILE>` — the only path the app ever writes.
pub const REMOTE_PATH: &str = "sync/latest.iksync";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A WebDAV transport failure. The taxonomy is what the UI (Task 5) branches
/// on, so the discriminants must stay stable and human-readable.
///
/// No variant carries credentials: every message is produced by
/// `redact_secrets` before it is stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebDavError {
    /// No URL configured yet — a local "not set up" condition, not a bug.
    MissingConfig(String),
    /// The configured URL is not a usable http(s) URL.
    InvalidUrl(String),
    /// `latest.iksync` does not exist yet. FIRST SYNC — this is a normal
    /// outcome, never surfaced as a failure.
    RemoteNotFound,
    /// HTTP 401 / 407, or the server rejected basic auth.
    AuthenticationFailed,
    /// HTTP 403: authenticated, but not allowed to do this.
    PermissionDenied,
    /// DNS / TCP / TLS / timeout — nothing reached the server.
    ConnectionFailed(String),
    /// The server answered, but with a status this client does not model
    /// (405 method not allowed, 409 missing parent, 5xx, ...).
    Protocol { status: u16, message: String },
    /// HTTP 412: a precondition (`If-Match` / `If-None-Match`) failed, i.e.
    /// the remote changed between our GET and our PUT.
    PreconditionFailed,
}

impl WebDavError {
    /// Stable, machine-readable category for the UI.
    pub fn code(&self) -> &'static str {
        match self {
            WebDavError::MissingConfig(_) => "missingConfig",
            WebDavError::InvalidUrl(_) => "invalidUrl",
            WebDavError::RemoteNotFound => "remoteNotFound",
            WebDavError::AuthenticationFailed => "authenticationFailed",
            WebDavError::PermissionDenied => "permissionDenied",
            WebDavError::ConnectionFailed(_) => "connectionFailed",
            WebDavError::Protocol { .. } => "webdavProtocolError",
            WebDavError::PreconditionFailed => "preconditionFailed",
        }
    }
}

impl std::fmt::Display for WebDavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebDavError::MissingConfig(m) => write!(f, "{m}"),
            WebDavError::InvalidUrl(m) => write!(f, "WebDAV 地址无效：{m}"),
            WebDavError::RemoteNotFound => write!(f, "远端尚未存在同步文件。"),
            WebDavError::AuthenticationFailed => {
                write!(f, "WebDAV 认证失败，请检查用户名与密码。")
            }
            WebDavError::PermissionDenied => {
                write!(f, "WebDAV 服务器拒绝了该操作，请检查目录权限。")
            }
            WebDavError::ConnectionFailed(m) => write!(f, "无法连接到 WebDAV 服务器：{m}"),
            WebDavError::Protocol { status, message } => {
                write!(f, "WebDAV 服务器返回异常响应（HTTP {status}）：{message}")
            }
            WebDavError::PreconditionFailed => {
                write!(f, "远端同步文件已被其他设备更新，正在重新合并。")
            }
        }
    }
}

impl std::error::Error for WebDavError {}

// ---------------------------------------------------------------------------
// Credential redaction
// ---------------------------------------------------------------------------

/// Scrub everything that could carry a credential out of `text`:
///   1. every literal occurrence of the configured password,
///   2. any `scheme://user:pass@host` inline userinfo.
///
/// Used on *every* string that can reach the UI, a log line or an error
/// value. `secret` may be empty (unauthenticated WebDAV) — URLs are still
/// cleaned.
pub fn redact_secrets(text: &str, secret: &str) -> String {
    let mut out = text.to_string();
    if !secret.is_empty() {
        out = out.replace(secret, "***");
    }
    strip_url_userinfo(&out)
}

/// Replace the userinfo component of every URL in `text` with `***`.
/// Handles any number of URLs; a fragment without `@` is copied verbatim.
fn strip_url_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_at) = rest.find("://") {
        let head = &rest[..scheme_at + 3];
        let tail = &rest[scheme_at + 3..];
        // The authority ends at the first path / query / fragment delimiter
        // or whitespace (URLs inside prose).
        let authority_end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, remainder) = tail.split_at(authority_end);
        out.push_str(head);
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = remainder;
    }
    out.push_str(rest);
    out
}

/// Remove any inline `user:pass@` from a URL so it is safe to use in *actual
/// requests*. Credentials travel only through the `Authorization` header,
/// never the request URL — unlike [strip_url_userinfo] this leaves no `***`
/// placeholder, because a placeholder userinfo would itself be sent as bogus
/// basic-auth by the HTTP client.
fn clean_url_host(url: &str) -> String {
    if let Some(scheme_at) = url.find("://") {
        let (head, tail) = url.split_at(scheme_at + 3);
        let authority_end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, remainder) = tail.split_at(authority_end);
        if let Some(at) = authority.rfind('@') {
            return format!("{}{}", head, &authority[at + 1..]);
        }
    }
    url.to_string()
}

/// If the URL carries inline `user:pass@`, lift it into the auth fields when
/// the config left those fields empty, so credentials pasted into the URL
/// keep working instead of being silently dropped.
fn parse_inline_credentials(url: &str) -> (Option<String>, Option<String>) {
    if let Some(scheme_at) = url.find("://") {
        let tail = &url[scheme_at + 3..];
        let authority_end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        if let Some(at) = authority.rfind('@') {
            let userinfo = &authority[..at];
            let (user, pass) = match userinfo.rfind(':') {
                Some(colon) => (&userinfo[..colon], &userinfo[colon + 1..]),
                None => (userinfo, ""),
            };
            return (Some(user.to_string()), Some(pass.to_string()));
        }
    }
    (None, None)
}

// ---------------------------------------------------------------------------
// Transport contract
// ---------------------------------------------------------------------------

/// One object fetched from the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    pub bytes: Vec<u8>,
    /// Opaque ETag as reported by the server, used verbatim as the
    /// `If-Match` value on the following PUT. `None` when the server does
    /// not expose ETags (degrades to an unconditional PUT).
    pub etag: Option<String>,
}

/// Result of a successful PUT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PutResult {
    pub etag: Option<String>,
    /// `true` when the resource did not exist before this upload (201).
    pub created: bool,
}

/// Precondition attached to a PUT. This is the whole concurrency-protection
/// mechanism: no server-side lock, no coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutCondition {
    /// No precondition. Only used when the server gave us no ETag.
    Unconditional,
    /// `If-Match: <etag>` — the normal case. The server must reject the
    /// upload with 412 if the resource changed since our GET.
    IfMatch(String),
    /// `If-None-Match: *` — create-only, used for the first sync where the
    /// remote had no snapshot. Turns "another device created it while we
    /// were merging" into a retryable 412 instead of a silent overwrite.
    IfNoneMatchStar,
}

/// The WebDAV transport surface. Both the real reqwest client and the
/// in-memory test double implement it, so the whole sync algorithm is
/// exercised without a network.
///
/// `path` arguments are relative to the configured WebDAV root; `""` means
/// the root itself.
pub trait WebDavTransport {
    /// GET `<root>/sync/latest.iksync`. `RemoteNotFound` when it is absent.
    fn get_latest(&self) -> BoxFuture<'_, Result<RemoteObject, WebDavError>>;
    /// PUT `<root>/sync/latest.iksync`.
    fn put_latest<'a>(
        &'a self,
        body: &'a [u8],
        condition: PutCondition,
    ) -> BoxFuture<'a, Result<PutResult, WebDavError>>;
    /// Make sure `<root>/sync/` exists (MKCOL; 405 = already there).
    fn ensure_dir(&self) -> BoxFuture<'_, Result<(), WebDavError>>;
    /// OPTIONS — returns the advertised `DAV` compliance tokens (may be
    /// empty when the server does not answer OPTIONS).
    fn options<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<String>, WebDavError>>;
    /// PROPFIND (Depth: 0) — `RemoteNotFound` when the resource is absent.
    fn propfind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), WebDavError>>;
    /// MKCOL — create a collection.
    fn mkcol<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), WebDavError>>;
}

// ---------------------------------------------------------------------------
// Real client
// ---------------------------------------------------------------------------

/// The production transport. Holds the password only to authenticate and to
/// scrub it out of error strings.
///
/// `Debug` is hand-written so the password can never be printed by an
/// accidental `{:?}` — the same reason it is absent from every error.
pub struct ReqwestWebDavClient {
    http: reqwest::Client,
    /// Configured root, trailing slashes removed.
    root: String,
    username: String,
    /// NEVER logged, never serialized, never returned. Only used by
    /// `redact` to scrub error text.
    secret: String,
}

impl std::fmt::Debug for ReqwestWebDavClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestWebDavClient")
            // `root` is already userinfo-free; `self.redact` is a belt-and-
            // braces scrub in case a password leaked in by some other path.
            .field("root", &self.redact(&self.root))
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

impl ReqwestWebDavClient {
    pub fn new(cfg: &WebDavConfig) -> Result<Self, WebDavError> {
        let raw = cfg.url.trim();
        if raw.is_empty() {
            return Err(WebDavError::MissingConfig(
                "尚未配置 WebDAV 地址，请先在设置中填写。".to_string(),
            ));
        }
        if !raw.starts_with("http://") && !raw.starts_with("https://") {
            return Err(WebDavError::InvalidUrl(
                "地址必须以 http:// 或 https:// 开头。".to_string(),
            ));
        }
        let (inline_user, inline_pass) = parse_inline_credentials(raw);
        let root = clean_url_host(raw.trim_end_matches('/'));
        let username = if cfg.username.is_empty() {
            inline_user.unwrap_or_default()
        } else {
            cfg.username.clone()
        };
        let secret = if cfg.password.is_empty() {
            inline_pass.unwrap_or_default()
        } else {
            cfg.password.clone()
        };
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| WebDavError::ConnectionFailed(redact_secrets(&e.to_string(), &secret)))?;
        Ok(Self {
            http,
            root,
            username,
            secret,
        })
    }

    /// Absolute URL for a path relative to the root (`""` = the root).
    pub fn url_for(&self, path: &str) -> String {
        if path.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{}", self.root, path)
        }
    }

    /// Absolute URL of the remote snapshot file.
    pub fn remote_file_url(&self) -> String {
        self.url_for(REMOTE_PATH)
    }

    /// Absolute URL of the remote snapshot directory.
    pub fn remote_dir_url(&self) -> String {
        self.url_for(REMOTE_DIR)
    }

    /// Scrub a string that may contain the password or an inline-credential
    /// URL before it becomes an error value / log line.
    fn redact(&self, text: &str) -> String {
        redact_secrets(text, &self.secret)
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.username.is_empty() && self.secret.is_empty() {
            request
        } else {
            request.basic_auth(&self.username, Some(&self.secret))
        }
    }

    /// Map an HTTP status onto the error taxonomy. `2xx` and `207
    /// Multi-Status` are success.
    fn check(&self, status: reqwest::StatusCode) -> Result<(), WebDavError> {
        let code = status.as_u16();
        match code {
            200..=299 => Ok(()),
            401 | 407 => Err(WebDavError::AuthenticationFailed),
            403 => Err(WebDavError::PermissionDenied),
            404 | 410 => Err(WebDavError::RemoteNotFound),
            405 => Err(WebDavError::Protocol {
                status: code,
                message: "服务器不支持该操作（Method Not Allowed）。".to_string(),
            }),
            409 => Err(WebDavError::Protocol {
                status: code,
                message: "父目录不存在，请先确认 WebDAV 根目录可写。".to_string(),
            }),
            412 => Err(WebDavError::PreconditionFailed),
            423 => Err(WebDavError::Protocol {
                status: code,
                message: "资源被锁定，请稍后重试。".to_string(),
            }),
            _ => Err(WebDavError::Protocol {
                status: code,
                message: "服务器返回了未预期的状态码。".to_string(),
            }),
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, WebDavError> {
        request
            .send()
            .await
            .map_err(|e| WebDavError::ConnectionFailed(self.redact(&e.to_string())))
    }
}

fn etag_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn dav_tokens_of(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get("dav")
        .and_then(|v| v.to_str().ok())
        .map(|raw| {
            raw.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn propfind_method() -> reqwest::Method {
    reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid HTTP method token")
}

fn mkcol_method() -> reqwest::Method {
    reqwest::Method::from_bytes(b"MKCOL").expect("MKCOL is a valid HTTP method token")
}

impl WebDavTransport for ReqwestWebDavClient {
    fn get_latest(&self) -> BoxFuture<'_, Result<RemoteObject, WebDavError>> {
        Box::pin(async move {
            let response = self.send(self.auth(self.http.get(self.remote_file_url()))).await?;
            let status = response.status();
            if status.as_u16() == 404 || status.as_u16() == 410 {
                return Err(WebDavError::RemoteNotFound);
            }
            self.check(status)?;
            let etag = etag_of(&response);
            let bytes = response
                .bytes()
                .await
                .map_err(|e| WebDavError::ConnectionFailed(self.redact(&e.to_string())))?
                .to_vec();
            Ok(RemoteObject { bytes, etag })
        })
    }

    fn put_latest<'a>(
        &'a self,
        body: &'a [u8],
        condition: PutCondition,
    ) -> BoxFuture<'a, Result<PutResult, WebDavError>> {
        Box::pin(async move {
            let mut request = self.http.put(self.remote_file_url());
            match &condition {
                PutCondition::Unconditional => {}
                PutCondition::IfMatch(etag) => {
                    request = request.header(IF_MATCH, etag.as_str());
                }
                PutCondition::IfNoneMatchStar => {
                    request = request.header(IF_NONE_MATCH, "*");
                }
            }
            let response = self
                .send(self.auth(request).body(body.to_vec()))
                .await?;
            let status = response.status();
            if status.as_u16() == 412 {
                return Err(WebDavError::PreconditionFailed);
            }
            self.check(status)?;
            Ok(PutResult {
                etag: etag_of(&response),
                created: status.as_u16() == 201,
            })
        })
    }

    fn ensure_dir(&self) -> BoxFuture<'_, Result<(), WebDavError>> {
        Box::pin(async move {
            let response = self
                .send(
                    self.auth(
                        self.http
                            .request(mkcol_method(), self.remote_dir_url()),
                    ),
                )
                .await?;
            let status = response.status();
            match status.as_u16() {
                // 200/201 = created, 405 = already exists (RFC 4918).
                200..=299 | 405 => Ok(()),
                401 | 407 => Err(WebDavError::AuthenticationFailed),
                403 => Err(WebDavError::PermissionDenied),
                409 => Err(WebDavError::Protocol {
                    status: 409,
                    message: "父目录不存在，无法创建 sync 目录。".to_string(),
                }),
                other => Err(WebDavError::Protocol {
                    status: other,
                    message: "创建 sync 目录失败。".to_string(),
                }),
            }
        })
    }

    fn options<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<String>, WebDavError>> {
        Box::pin(async move {
            let response = self
                .send(self.auth(self.http.request(reqwest::Method::OPTIONS, self.url_for(path))))
                .await?;
            let status = response.status();
            match status.as_u16() {
                200..=299 => Ok(dav_tokens_of(&response)),
                // A server that does not implement OPTIONS is still a
                // perfectly valid WebDAV server; PROPFIND decides.
                405 | 501 => Ok(Vec::new()),
                401 | 407 => Err(WebDavError::AuthenticationFailed),
                403 => Err(WebDavError::PermissionDenied),
                404 | 410 => Err(WebDavError::RemoteNotFound),
                other => Err(WebDavError::Protocol {
                    status: other,
                    message: "探测服务器能力失败。".to_string(),
                }),
            }
        })
    }

    fn propfind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), WebDavError>> {
        Box::pin(async move {
            let response = self
                .send(
                    self.auth(
                        self.http
                            .request(propfind_method(), self.url_for(path))
                            .header("Depth", "0"),
                    ),
                )
                .await?;
            let status = response.status();
            match status.as_u16() {
                // 207 Multi-Status is inside 2xx; some servers answer 200.
                200..=299 => Ok(()),
                401 | 407 => Err(WebDavError::AuthenticationFailed),
                403 => Err(WebDavError::PermissionDenied),
                404 | 410 => Err(WebDavError::RemoteNotFound),
                other => Err(WebDavError::Protocol {
                    status: other,
                    message: "PROPFIND 探测失败，该地址可能不是 WebDAV 端点。".to_string(),
                }),
            }
        })
    }

    fn mkcol<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), WebDavError>> {
        Box::pin(async move {
            let response = self
                .send(self.auth(self.http.request(mkcol_method(), self.url_for(path))))
                .await?;
            let status = response.status();
            match status.as_u16() {
                200..=299 | 405 => Ok(()),
                401 | 407 => Err(WebDavError::AuthenticationFailed),
                403 => Err(WebDavError::PermissionDenied),
                409 => Err(WebDavError::Protocol {
                    status: 409,
                    message: "父目录不存在。".to_string(),
                }),
                other => Err(WebDavError::Protocol {
                    status: other,
                    message: "创建目录失败。".to_string(),
                }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Test connection
// ---------------------------------------------------------------------------

/// Result surfaced by the "Test Connection" button. Always returned as a
/// successful command; failures are described by `success: false` +
/// `error_kind` so the UI can branch without parsing prose.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDavTestResult {
    pub success: bool,
    /// Round-trip time of the whole probe, in milliseconds.
    pub latency_ms: u64,
    /// Human-readable, credential-free summary.
    pub message: String,
    /// `WebDavError::code()` of the failure; empty on success.
    pub error_kind: String,
}

/// Probe the configured WebDAV endpoint.
///
/// A plain `GET /` returning 200 is NOT enough — plenty of HTTP servers
/// (including plain file hosting and login pages) answer that without being
/// WebDAV at all. The probe therefore:
///
///   1. OPTIONS the root — informational (records the advertised `DAV`
///      compliance classes). 401/403 fail fast; 405/501 is tolerated.
///   2. PROPFIND (Depth: 0) the root — this is the real WebDAV check: it
///      proves the URL is reachable, the endpoint speaks WebDAV, and the
///      credentials are accepted.
///   3. PROPFIND `<root>/sync` — when it is missing, MKCOL it. This is the
///      only write the probe performs and it never touches
///      `latest.iksync`, so an existing snapshot cannot be damaged by a
///      connection test.
pub async fn test_connection<T: WebDavTransport + ?Sized>(transport: &T) -> WebDavTestResult {
    let started = Instant::now();
    let outcome = probe(transport).await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(dav) => {
            let message = if dav.is_empty() {
                format!("连接成功（{latency_ms}ms），同步目录可用。")
            } else {
                format!(
                    "连接成功（{latency_ms}ms）。服务器支持 WebDAV {}，同步目录可用。",
                    dav.join(", ")
                )
            };
            WebDavTestResult {
                success: true,
                latency_ms,
                message,
                error_kind: String::new(),
            }
        }
        Err(e) => WebDavTestResult {
            success: false,
            latency_ms,
            message: e.to_string(),
            error_kind: e.code().to_string(),
        },
    }
}

/// Returns the advertised DAV compliance classes (possibly empty).
async fn probe<T: WebDavTransport + ?Sized>(transport: &T) -> Result<Vec<String>, WebDavError> {
    // 1. Capability probe (informational).
    let dav = match transport.options("").await {
        Ok(tokens) => tokens,
        // Auth is fatal; anything else is decided by PROPFIND.
        Err(WebDavError::AuthenticationFailed) => return Err(WebDavError::AuthenticationFailed),
        Err(WebDavError::PermissionDenied) => return Err(WebDavError::PermissionDenied),
        Err(_) => Vec::new(),
    };

    // 2. The real check: URL reachable + WebDAV endpoint + credentials.
    transport.propfind("").await?;

    // 3. Make sure the snapshot directory is usable.
    match transport.propfind(REMOTE_DIR).await {
        Ok(()) => {}
        Err(WebDavError::RemoteNotFound) => transport.mkcol(REMOTE_DIR).await?,
        Err(e) => return Err(e),
    }

    Ok(dav)
}

// ---------------------------------------------------------------------------
// In-memory test double
// ---------------------------------------------------------------------------

/// A minimal WebDAV server in memory, used to exercise the whole sync
/// algorithm (including ETag preconditions and concurrent writers) inside
/// `cargo test` with no network and no external process.
///
/// Behaviour faithful to a *supporting* server by default:
///   * `sync/latest.iksync` PUT/GET with ETags (`v1`, `v2`, ...),
///   * `If-Match` / `If-None-Match: *` enforced (412 on mismatch),
///   * MKCOL for the `sync` directory, 409 when it is missing.
///
/// Switches let a test emulate the degraded cases (`no_etag`,
/// `ignore_preconditions`) and the failure cases (`get_fault`,
/// `put_fault_once`, `propfind_fault`).
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    pub struct FakeWebDav {
        pub files: Mutex<HashMap<String, Vec<u8>>>,
        pub etags: Mutex<HashMap<String, String>>,
        pub dirs: Mutex<HashSet<String>>,
        pub get_count: Mutex<usize>,
        pub put_count: Mutex<usize>,
        pub etag_seq: Mutex<u64>,
        /// Returned by every GET (consumed but not cleared).
        pub get_fault: Mutex<Option<WebDavError>>,
        /// Returned by the next PUT, then cleared.
        pub put_fault_once: Mutex<Option<WebDavError>>,
        /// Returned by every PROPFIND.
        pub propfind_fault: Mutex<Option<WebDavError>>,
        /// Returned by every OPTIONS.
        pub options_fault: Mutex<Option<WebDavError>>,
        /// Returned by every MKCOL (unwritable directory).
        pub mkcol_fault: Mutex<Option<WebDavError>>,
        /// Server never sends an ETag.
        pub no_etag: bool,
        /// Server ignores `If-Match` / `If-None-Match` (last-writer-wins).
        pub ignore_preconditions: bool,
        /// Advertised DAV compliance classes.
        pub dav: Mutex<Vec<String>>,
        /// Bytes another device commits immediately *after* our next GET —
        /// this is what turns the following PUT into a 412. Consumed once.
        pub concurrent_write: Mutex<Option<Vec<u8>>>,
        /// Same, but re-applied after *every* GET (models a peer that keeps
        /// committing; used to exercise the bounded retry).
        pub sticky_concurrent_write: Mutex<Option<Vec<u8>>>,
    }

    impl Default for FakeWebDav {
        fn default() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                etags: Mutex::new(HashMap::new()),
                dirs: Mutex::new(HashSet::new()),
                get_count: Mutex::new(0),
                put_count: Mutex::new(0),
                etag_seq: Mutex::new(0),
                get_fault: Mutex::new(None),
                put_fault_once: Mutex::new(None),
                propfind_fault: Mutex::new(None),
                options_fault: Mutex::new(None),
                mkcol_fault: Mutex::new(None),
                no_etag: false,
                ignore_preconditions: false,
                dav: Mutex::new(vec!["1".to_string(), "2".to_string()]),
                concurrent_write: Mutex::new(None),
                sticky_concurrent_write: Mutex::new(None),
            }
        }
    }

    impl FakeWebDav {
        pub fn new() -> Self {
            Self::default()
        }

        /// A server that already has the `sync` directory but no snapshot.
        pub fn with_dir() -> Self {
            let s = Self::new();
            s.dirs.lock().unwrap().insert(REMOTE_DIR.to_string());
            s
        }

        /// A server whose `sync/latest.iksync` already holds `bytes`.
        pub fn with_remote(bytes: Vec<u8>) -> Self {
            let s = Self::with_dir();
            s.write_raw(bytes);
            s
        }

        fn next_etag(&self) -> String {
            let mut seq = self.etag_seq.lock().unwrap();
            *seq += 1;
            format!("v{seq}")
        }

        /// Commit `bytes` as the remote snapshot and bump its ETag — the
        /// equivalent of another device finishing its own sync.
        pub         fn write_raw(&self, bytes: Vec<u8>) {
            // A peer that uploads a snapshot has the directory already.
            self.dirs.lock().unwrap().insert(REMOTE_DIR.to_string());
            self.files
                .lock()
                .unwrap()
                .insert(REMOTE_PATH.to_string(), bytes);
            if self.no_etag {
                self.etags.lock().unwrap().remove(REMOTE_PATH);
            } else {
                let etag = self.next_etag();
                self.etags
                    .lock()
                    .unwrap()
                    .insert(REMOTE_PATH.to_string(), etag);
            }
        }

        /// The remote snapshot as the next device would download it.
        pub fn remote_bytes(&self) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(REMOTE_PATH).cloned()
        }

        pub fn remote_etag(&self) -> Option<String> {
            self.etags.lock().unwrap().get(REMOTE_PATH).cloned()
        }

        /// Emulate another device committing `bytes` right after our next
        /// GET (so our ETag goes stale and the PUT must be retried).
        pub fn schedule_concurrent_write(&self, bytes: Vec<u8>) {
            *self.concurrent_write.lock().unwrap() = Some(bytes);
        }

        /// Like `schedule_concurrent_write`, but re-armed after every GET.
        pub fn schedule_concurrent_write_forever(&self, bytes: Vec<u8>) {
            *self.sticky_concurrent_write.lock().unwrap() = Some(bytes);
        }

        pub fn get_calls(&self) -> usize {
            *self.get_count.lock().unwrap()
        }

        pub fn put_calls(&self) -> usize {
            *self.put_count.lock().unwrap()
        }

        pub fn has_dir(&self, path: &str) -> bool {
            self.dirs.lock().unwrap().contains(path)
        }
    }

    impl WebDavTransport for FakeWebDav {
        fn get_latest(&self) -> BoxFuture<'_, Result<RemoteObject, WebDavError>> {
            Box::pin(async move {
                *self.get_count.lock().unwrap() += 1;
                if let Some(e) = self.get_fault.lock().unwrap().clone() {
                    return Err(e);
                }
                // Read the state as it is *now*...
                let result = if !self.dirs.lock().unwrap().contains(REMOTE_DIR) {
                    Err(WebDavError::RemoteNotFound)
                } else {
                    match self.files.lock().unwrap().get(REMOTE_PATH).cloned() {
                        None => Err(WebDavError::RemoteNotFound),
                        Some(bytes) => Ok(RemoteObject {
                            bytes,
                            etag: self.etags.lock().unwrap().get(REMOTE_PATH).cloned(),
                        }),
                    }
                };
                // ...then let another device commit while we merge, which is
                // what makes the ETag we just read stale.
                if let Some(bytes) = self.concurrent_write.lock().unwrap().take() {
                    self.write_raw(bytes);
                }
                if let Some(bytes) = self.sticky_concurrent_write.lock().unwrap().clone() {
                    self.write_raw(bytes);
                }
                result
            })
        }

        fn put_latest<'a>(
            &'a self,
            body: &'a [u8],
            condition: PutCondition,
        ) -> BoxFuture<'a, Result<PutResult, WebDavError>> {
            Box::pin(async move {
                *self.put_count.lock().unwrap() += 1;
                if let Some(e) = self.put_fault_once.lock().unwrap().take() {
                    return Err(e);
                }
                if !self.dirs.lock().unwrap().contains(REMOTE_DIR) {
                    return Err(WebDavError::Protocol {
                        status: 409,
                        message: "父目录不存在".to_string(),
                    });
                }
                let exists = self.files.lock().unwrap().contains_key(REMOTE_PATH);
                let current_etag = self.etags.lock().unwrap().get(REMOTE_PATH).cloned();
                if !self.ignore_preconditions {
                    match &condition {
                        PutCondition::Unconditional => {}
                        PutCondition::IfNoneMatchStar => {
                            if exists {
                                return Err(WebDavError::PreconditionFailed);
                            }
                        }
                        PutCondition::IfMatch(expected) => {
                            if current_etag.as_deref() != Some(expected.as_str()) {
                                return Err(WebDavError::PreconditionFailed);
                            }
                        }
                    }
                }
                self.files
                    .lock()
                    .unwrap()
                    .insert(REMOTE_PATH.to_string(), body.to_vec());
                let etag = if self.no_etag {
                    self.etags.lock().unwrap().remove(REMOTE_PATH);
                    None
                } else {
                    let etag = self.next_etag();
                    self.etags
                        .lock()
                        .unwrap()
                        .insert(REMOTE_PATH.to_string(), etag.clone());
                    Some(etag)
                };
                Ok(PutResult {
                    etag,
                    created: !exists,
                })
            })
        }

        fn ensure_dir(&self) -> BoxFuture<'_, Result<(), WebDavError>> {
            Box::pin(async move {
                self.dirs.lock().unwrap().insert(REMOTE_DIR.to_string());
                Ok(())
            })
        }

        fn options<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Vec<String>, WebDavError>> {
            Box::pin(async move {
                if let Some(e) = self.options_fault.lock().unwrap().clone() {
                    return Err(e);
                }
                if !path.is_empty() && !self.has_dir(path) {
                    return Err(WebDavError::RemoteNotFound);
                }
                Ok(self.dav.lock().unwrap().clone())
            })
        }

        fn propfind<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), WebDavError>> {
            Box::pin(async move {
                if let Some(e) = self.propfind_fault.lock().unwrap().clone() {
                    return Err(e);
                }
                if path.is_empty() {
                    return Ok(());
                }
                if self.has_dir(path) {
                    Ok(())
                } else {
                    Err(WebDavError::RemoteNotFound)
                }
            })
        }

        fn mkcol<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), WebDavError>> {
            Box::pin(async move {
                if let Some(e) = self.mkcol_fault.lock().unwrap().clone() {
                    return Err(e);
                }
                self.dirs.lock().unwrap().insert(path.to_string());
                Ok(())
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::testing::FakeWebDav;
    use super::*;

    fn cfg(url: &str, user: &str, pw: &str) -> WebDavConfig {
        WebDavConfig {
            url: url.to_string(),
            username: user.to_string(),
            password: pw.to_string(),
        }
    }

    fn client(url: &str) -> ReqwestWebDavClient {
        ReqwestWebDavClient::new(&cfg(url, "alice", "s3cr3t")).unwrap()
    }

    // ----- configuration / URL handling -----

    #[test]
    fn rejects_empty_url() {
        match ReqwestWebDavClient::new(&cfg("   ", "u", "p")) {
            Err(WebDavError::MissingConfig(_)) => {}
            other => panic!("expected MissingConfig, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_http_url() {
        match ReqwestWebDavClient::new(&cfg("ftp://dav.example.com", "u", "p")) {
            Err(WebDavError::InvalidUrl(_)) => {}
            other => panic!("expected InvalidUrl, got {other:?}"),
        }
    }

    #[test]
    fn url_layout_is_root_slash_sync_slash_latest() {
        let c = client("https://dav.example.com/dav/files/alice/Sisyphus/");
        assert_eq!(
            c.remote_file_url(),
            "https://dav.example.com/dav/files/alice/Sisyphus/sync/latest.iksync"
        );
        assert_eq!(
            c.remote_dir_url(),
            "https://dav.example.com/dav/files/alice/Sisyphus/sync"
        );
        // Root URL keeps no trailing slash.
        assert_eq!(
            c.url_for(""),
            "https://dav.example.com/dav/files/alice/Sisyphus"
        );
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(WebDavError::RemoteNotFound.code(), "remoteNotFound");
        assert_eq!(
            WebDavError::AuthenticationFailed.code(),
            "authenticationFailed"
        );
        assert_eq!(WebDavError::PermissionDenied.code(), "permissionDenied");
        assert_eq!(
            WebDavError::ConnectionFailed("x".into()).code(),
            "connectionFailed"
        );
        assert_eq!(
            WebDavError::Protocol { status: 500, message: "x".into() }.code(),
            "webdavProtocolError"
        );
        assert_eq!(WebDavError::PreconditionFailed.code(), "preconditionFailed");
    }

    // ----- credential redaction -----

    /// Requirement 12: the password must not appear in a snapshot, in the
    /// DB, in logs, or in an error message. Here we cover the error path.
    #[test]
    fn redaction_removes_password_and_inline_userinfo() {
        let text = "GET https://alice:s3cr3t@dav.example.com/dav failed with s3cr3t";
        let out = redact_secrets(text, "s3cr3t");
        assert!(!out.contains("s3cr3t"), "leaked credential: {out}");
        assert!(out.contains("***@dav.example.com/dav"), "got: {out}");
        assert!(out.contains("***"), "got: {out}");
    }

    #[test]
    fn redaction_handles_multiple_urls_and_no_credentials() {
        let out = redact_secrets(
            "https://a:b@one.example.com/x and https://plain.example.com/y",
            "b",
        );
        assert!(!out.contains("a:b@"), "got: {out}");
        assert!(out.contains("https://plain.example.com/y"), "got: {out}");
        // No userinfo -> untouched.
        assert_eq!(redact_secrets("https://x.example.com", ""), "https://x.example.com");
    }

    #[test]
    fn client_redacts_through_its_own_secret() {
        let c = client("https://dav.example.com/dav");
        let out = c.redact("boom s3cr3t at https://u:s3cr3t@dav.example.com");
        assert!(!out.contains("s3cr3t"), "leaked: {out}");
    }

    #[test]
    fn client_display_never_prints_a_password() {
        // The client holds the secret only for auth; nothing in Debug/Display
        // of the *error* surface can carry it.
        let e = WebDavError::ConnectionFailed(
            client("https://dav.example.com").redact("failed for s3cr3t"),
        );
        assert!(!e.to_string().contains("s3cr3t"));
    }

    // ----- test connection -----

    #[tokio::test]
    async fn test_connection_succeeds_on_a_real_webdav_endpoint() {
        let server = FakeWebDav::with_dir();
        let result = test_connection(&server).await;
        assert!(result.success, "{}", result.message);
        assert!(result.error_kind.is_empty());
        assert!(result.message.contains("1, 2"), "{}", result.message);
        assert!(result.message.contains("ms"), "{}", result.message);
    }

    #[tokio::test]
    async fn test_connection_creates_the_sync_directory() {
        let server = FakeWebDav::new();
        assert!(!server.has_dir(REMOTE_DIR));
        let result = test_connection(&server).await;
        assert!(result.success, "{}", result.message);
        assert!(server.has_dir(REMOTE_DIR));
    }

    /// Test connection must never create, modify or delete `latest.iksync`.
    #[tokio::test]
    async fn test_connection_leaves_latest_untouched() {
        let server = FakeWebDav::with_remote(b"{\"formatVersion\":1}".to_vec());
        let before = server.remote_bytes();
        let result = test_connection(&server).await;
        assert!(result.success);
        assert_eq!(server.remote_bytes(), before);
        assert_eq!(server.put_calls(), 0);
    }

    #[tokio::test]
    async fn test_connection_reports_authentication_failure() {
        let server = FakeWebDav::with_dir();
        *server.propfind_fault.lock().unwrap() = Some(WebDavError::AuthenticationFailed);
        let result = test_connection(&server).await;
        assert!(!result.success);
        assert_eq!(result.error_kind, "authenticationFailed");
        assert!(!result.message.is_empty());
    }

    /// Root readable, `sync/` missing and not creatable -> the probe must
    /// report the failure instead of pretending everything is fine.
    #[tokio::test]
    async fn test_connection_reports_permission_denied_on_mkcol() {
        let server = FakeWebDav::new();
        *server.mkcol_fault.lock().unwrap() = Some(WebDavError::PermissionDenied);
        let result = test_connection(&server).await;
        assert!(!result.success);
        assert_eq!(result.error_kind, "permissionDenied");
        assert!(!server.has_dir(REMOTE_DIR));
    }

    #[tokio::test]
    async fn test_connection_reports_connection_failure() {
        let server = FakeWebDav::with_dir();
        *server.propfind_fault.lock().unwrap() =
            Some(WebDavError::ConnectionFailed("dns 解析失败".into()));
        let result = test_connection(&server).await;
        assert!(!result.success);
        assert_eq!(result.error_kind, "connectionFailed");
    }

    /// A server that refuses OPTIONS is still usable: PROPFIND decides.
    #[tokio::test]
    async fn test_connection_tolerates_missing_options_support() {
        let server = FakeWebDav::with_dir();
        *server.options_fault.lock().unwrap() =
            Some(WebDavError::Protocol { status: 405, message: "nope".into() });
        *server.dav.lock().unwrap() = Vec::new();
        let result = test_connection(&server).await;
        assert!(result.success, "{}", result.message);
    }

    /// ...but an auth failure on OPTIONS must not be masked by PROPFIND.
    #[tokio::test]
    async fn test_connection_does_not_mask_auth_failure() {
        let server = FakeWebDav::with_dir();
        *server.options_fault.lock().unwrap() = Some(WebDavError::AuthenticationFailed);
        let result = test_connection(&server).await;
        assert!(!result.success);
        assert_eq!(result.error_kind, "authenticationFailed");
    }

    // ----- transport-level behaviour of the fake (guards the guard) -----

    #[tokio::test]
    async fn fake_enforces_if_match_and_reports_stale_etag() {
        let server = FakeWebDav::with_remote(b"one".to_vec());
        let first = server.get_latest().await.unwrap();
        // Another device overwrites the file.
        server.write_raw(b"two".to_vec());
        let result = server
            .put_latest(b"merged", PutCondition::IfMatch(first.etag.clone().unwrap()))
            .await;
        assert_eq!(result, Err(WebDavError::PreconditionFailed));
        // With the fresh ETag the same upload succeeds.
        let fresh = server.get_latest().await.unwrap();
        assert!(
            server
                .put_latest(b"merged", PutCondition::IfMatch(fresh.etag.unwrap()))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn fake_if_none_match_star_only_creates() {
        let server = FakeWebDav::with_dir();
        assert!(
            server
                .put_latest(b"a", PutCondition::IfNoneMatchStar)
                .await
                .is_ok()
        );
        assert_eq!(
            server.put_latest(b"b", PutCondition::IfNoneMatchStar).await,
            Err(WebDavError::PreconditionFailed)
        );
    }

    #[tokio::test]
    async fn fake_reports_404_as_remote_not_found() {
        let server = FakeWebDav::with_dir();
        assert_eq!(server.get_latest().await, Err(WebDavError::RemoteNotFound));
    }
}
