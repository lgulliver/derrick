//! Bearer-token authentication and per-workspace authorization for the hub
//! (D83).
//!
//! When `hub.yaml` carries an `auth` section with at least one token, [`serve`]
//! wraps the rmcp [`StreamableHttpService`] in an axum middleware
//! ([`require_bearer`]) that:
//!
//! 1. reads the `Authorization: Bearer <token>` header,
//! 2. matches it against the configured tokens in **constant time**
//!    ([`AuthRegistry::resolve`], via [`subtle`]),
//! 3. on success injects the resolved [`Principal`] into the HTTP request
//!    extensions and calls the inner service, or
//! 4. on a missing/unknown token returns `401 Unauthorized` before the request
//!    ever reaches a tool handler.
//!
//! rmcp 1.7 copies the full `http::request::Parts` (including those extensions)
//! into each tool call's [`RequestContext`](rmcp::service::RequestContext)
//! extensions, so a `#[tool]` handler recovers the principal with
//! [`Principal::from_parts`] and authorizes the call with
//! [`Principal::authorize`]. This is the mechanism rmcp's own
//! `StreamableHttpService` docs prescribe for passing per-request state to
//! handlers.
//!
//! [`serve`]: crate::serve
//! [`StreamableHttpService`]: rmcp::transport::streamable_http_server::StreamableHttpService

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{StatusCode, request::Parts};
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::config::{AuthConfig, Capability, WorkspaceScope};

/// An authenticated caller: the workspaces and capabilities granted to the
/// bearer token that authenticated this request. Built once from
/// [`AuthConfig`] and cloned into each authorized request's extensions; it
/// never carries the token secret.
#[derive(Clone, Debug)]
pub struct Principal {
    scope: WorkspaceScope,
    capabilities: Arc<[Capability]>,
}

/// Why a tool call is refused authorization, kept distinct from authentication
/// (which fails earlier at the HTTP layer with a 401).
#[derive(Debug, PartialEq, Eq)]
pub enum AuthzError {
    /// The principal's scope does not include the requested workspace.
    ForbiddenWorkspace,
    /// The principal lacks the capability the tool requires.
    MissingCapability(Capability),
}

impl Principal {
    /// Whether this principal's scope includes `workspace`.
    fn allows_workspace(&self, workspace: &str) -> bool {
        match &self.scope {
            WorkspaceScope::All => true,
            WorkspaceScope::Ids(ids) => ids.iter().any(|id| id == workspace),
        }
    }

    /// Whether this principal holds `capability`.
    fn holds(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Authorize a tool call: the principal must reach `workspace` *and* hold
    /// `capability`. The workspace check is applied first so a token never
    /// learns which capabilities gate a workspace it cannot see.
    pub fn authorize(&self, workspace: &str, capability: Capability) -> Result<(), AuthzError> {
        if !self.allows_workspace(workspace) {
            return Err(AuthzError::ForbiddenWorkspace);
        }
        if !self.holds(capability) {
            return Err(AuthzError::MissingCapability(capability));
        }
        Ok(())
    }

    /// Recover the principal injected by [`require_bearer`] from the HTTP
    /// request `Parts` that rmcp threads into the tool-call context. `None`
    /// means the request was not authenticated — which, when auth is enabled,
    /// cannot happen for a request that reached a handler (the middleware would
    /// have returned 401 first), so handlers treat `None` as "auth disabled".
    pub fn from_parts(parts: &Parts) -> Option<&Principal> {
        parts.extensions.get::<Principal>()
    }
}

/// A built token-to-[`Principal`] lookup. Construction validates that each
/// token secret is non-empty; the registry is consulted in constant time so
/// the bearer check is not timing-attackable.
#[derive(Clone)]
pub struct AuthRegistry {
    // One entry per configured token. Each stores a fixed-length BLAKE3 digest
    // of the secret (not the raw, variable-length secret): the constant-time
    // compare in `resolve` then runs over equal-length inputs, so timing leaks
    // neither the matched position nor the secret's length. Duplicate secrets
    // are already rejected by `HubConfig::validate`.
    entries: Arc<[AuthEntry]>,
}

/// One resolved token: the digest we compare against and the principal it
/// grants. Never stores the raw secret.
#[derive(Clone)]
struct AuthEntry {
    token_digest: [u8; 32],
    principal: Principal,
}

/// Fixed-length digest of a token secret, so comparisons are length-independent.
fn token_digest(token: &[u8]) -> [u8; 32] {
    *blake3::hash(token).as_bytes()
}

impl AuthRegistry {
    /// Build a registry from an [`AuthConfig`]. Tokens are assumed already
    /// validated by [`HubConfig::validate`](crate::HubConfig::validate)
    /// (non-empty, unique, known scopes); this only materializes the lookup.
    pub fn build(auth: &AuthConfig) -> Self {
        let entries: Vec<AuthEntry> = auth
            .tokens
            .iter()
            .map(|token| AuthEntry {
                token_digest: token_digest(token.token.as_bytes()),
                principal: Principal {
                    // Single source of scope parsing (shared with validate()): a
                    // mixed `["*", id]` scope is rejected there, and if it ever
                    // reaches here unvalidated we fail closed (deny-all) rather
                    // than collapsing to `All`.
                    scope: token
                        .scope()
                        .unwrap_or_else(|_| WorkspaceScope::Ids(Vec::new())),
                    capabilities: token.capabilities.clone().into(),
                },
            })
            .collect();
        Self {
            entries: entries.into(),
        }
    }

    /// Resolve a presented secret to its [`Principal`], comparing fixed-length
    /// digests of every configured secret in constant time. Returns `None` for
    /// an unknown secret. The token value is never logged. Comparing digests
    /// (rather than the raw secrets) keeps the timing independent of the
    /// presented token's length as well as its content and position.
    fn resolve(&self, presented: &str) -> Option<Principal> {
        let presented = token_digest(presented.as_bytes());
        let mut matched: Option<&Principal> = None;
        // Iterate every entry with an equal-length constant-time compare; we do
        // not break on the first hit, to keep timing independent of position.
        for entry in self.entries.iter() {
            let equal: bool = entry
                .token_digest
                .as_slice()
                .ct_eq(presented.as_slice())
                .into();
            if equal {
                matched = Some(&entry.principal);
            }
        }
        matched.cloned()
    }
}

/// Extract a bearer token from an `Authorization` header value. The scheme is
/// matched case-insensitively per RFC 9110 §11.1 (`Bearer`, `bearer`, `BEARER`,
/// …), and the credential is trimmed.
fn bearer(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// axum middleware that enforces authentication when the hub is configured with
/// auth. A missing or unknown `Authorization: Bearer` token is rejected with
/// `401`; a valid token has its [`Principal`] inserted into the request
/// extensions for the tool handlers to authorize against. Failures are logged
/// at `warn`/`debug` without the token value.
pub async fn require_bearer(
    registry: Arc<AuthRegistry>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer);

    let Some(presented) = presented else {
        tracing::warn!("survey hub: rejecting request with missing bearer token");
        return StatusCode::UNAUTHORIZED.into_response_401();
    };

    match registry.resolve(presented) {
        Some(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        None => {
            tracing::warn!("survey hub: rejecting request with unknown bearer token");
            StatusCode::UNAUTHORIZED.into_response_401()
        }
    }
}

/// Small helper so the 401 path stays a one-liner without pulling in extra
/// response machinery.
trait Unauthorized {
    /// Build a `401 Unauthorized` response carrying the `WWW-Authenticate`
    /// challenge that RFC 7235 §3.1 requires on every 401.
    fn into_response_401(self) -> Response;
}

impl Unauthorized for StatusCode {
    fn into_response_401(self) -> Response {
        use axum::response::IntoResponse;
        let mut response = self.into_response();
        // RFC 7235 §3.1: a 401 MUST carry at least one challenge so conforming
        // clients can discover the expected scheme.
        response.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenConfig;

    fn registry(tokens: Vec<TokenConfig>) -> AuthRegistry {
        AuthRegistry::build(&AuthConfig { tokens })
    }

    fn token(secret: &str, workspaces: &[&str], caps: &[Capability]) -> TokenConfig {
        TokenConfig {
            token: secret.to_owned(),
            workspaces: workspaces.iter().map(|s| s.to_string()).collect(),
            capabilities: caps.to_vec(),
        }
    }

    #[test]
    fn resolve_matches_known_and_rejects_unknown() {
        let reg = registry(vec![token("sek", &["a"], &[Capability::Read])]);
        assert!(reg.resolve("sek").is_some());
        assert!(reg.resolve("nope").is_none());
        assert!(reg.resolve("").is_none());
    }

    #[test]
    fn authorize_checks_workspace_then_capability() {
        let p = registry(vec![token("s", &["a"], &[Capability::Read])])
            .resolve("s")
            .unwrap();
        assert_eq!(p.authorize("a", Capability::Read), Ok(()));
        assert_eq!(
            p.authorize("b", Capability::Read),
            Err(AuthzError::ForbiddenWorkspace)
        );
        assert_eq!(
            p.authorize("a", Capability::Refresh),
            Err(AuthzError::MissingCapability(Capability::Refresh))
        );
    }

    #[test]
    fn wildcard_scope_reaches_any_workspace() {
        let p = registry(vec![token(
            "s",
            &["*"],
            &[Capability::Read, Capability::Refresh],
        )])
        .resolve("s")
        .unwrap();
        assert_eq!(p.authorize("anything", Capability::Read), Ok(()));
        assert_eq!(p.authorize("other", Capability::Refresh), Ok(()));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer abc"), Some("abc"));
        // Scheme is case-insensitive (RFC 9110 §11.1).
        assert_eq!(bearer("BEARER abc"), Some("abc"));
        assert_eq!(bearer("bEaReR abc"), Some("abc"));
        assert_eq!(bearer("Bearer   "), None);
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
    }
}
