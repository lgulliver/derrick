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

use std::collections::HashMap;
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
    // Keyed by the raw secret only to dedup at build time; lookups iterate and
    // compare in constant time rather than hashing, so a near-miss secret is
    // not distinguishable by timing.
    entries: Arc<HashMap<String, Principal>>,
}

impl AuthRegistry {
    /// Build a registry from an [`AuthConfig`]. Tokens are assumed already
    /// validated by [`HubConfig::validate`](crate::HubConfig::validate)
    /// (non-empty, unique, known scopes); this only materializes the lookup.
    pub fn build(auth: &AuthConfig) -> Self {
        let mut entries = HashMap::with_capacity(auth.tokens.len());
        for token in &auth.tokens {
            entries.insert(
                token.token.clone(),
                Principal {
                    scope: scope_of(token),
                    capabilities: token.capabilities.clone().into(),
                },
            );
        }
        Self {
            entries: Arc::new(entries),
        }
    }

    /// Resolve a presented secret to its [`Principal`], comparing every
    /// configured secret in constant time. Returns `None` for an unknown
    /// secret. The token value is never logged.
    fn resolve(&self, presented: &str) -> Option<Principal> {
        let presented = presented.as_bytes();
        let mut matched: Option<&Principal> = None;
        // Iterate all entries with a constant-time compare so neither a length
        // mismatch nor an early byte difference shortens the loop. We do not
        // break on the first hit, to keep timing independent of position.
        for (secret, principal) in self.entries.iter() {
            let equal: bool = secret.as_bytes().ct_eq(presented).into();
            if equal {
                matched = Some(principal);
            }
        }
        matched.cloned()
    }
}

/// Resolve a token's declared workspaces into a [`WorkspaceScope`], mirroring
/// the validated config rule (a lone `"*"` is the wildcard).
fn scope_of(token: &crate::config::TokenConfig) -> WorkspaceScope {
    if token.workspaces.iter().any(|w| w == "*") {
        WorkspaceScope::All
    } else {
        WorkspaceScope::Ids(token.workspaces.clone())
    }
}

/// Extract a bearer token from an `Authorization` header value.
fn bearer(value: &str) -> Option<&str> {
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
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
    /// Build a bare `401 Unauthorized` response.
    fn into_response_401(self) -> Response;
}

impl Unauthorized for StatusCode {
    fn into_response_401(self) -> Response {
        use axum::response::IntoResponse;
        self.into_response()
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
        assert_eq!(bearer("Bearer   "), None);
        assert_eq!(bearer("Basic abc"), None);
    }
}
