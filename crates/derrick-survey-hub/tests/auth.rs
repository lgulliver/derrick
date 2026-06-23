//! HTTP-level authentication and per-workspace authorization tests for the
//! survey hub (D83).
//!
//! These drive the real rmcp streamable-HTTP transport (mirroring
//! `tests/routing.rs`) against a hub configured with bearer-token auth, using
//! real temp repos and real SQLite (house rule: no mocks). The bearer token is
//! supplied via the client transport's `auth_header`, which rmcp sends as
//! `Authorization: Bearer <token>`; the hub's middleware authenticates it and
//! the tool handlers authorize each call against the token's scope.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use derrick_survey_hub::{
    AuthConfig, Capability, Hub, HubConfig, TokenConfig, WorkspaceConfig, build_router,
};
use rmcp::model::CallToolRequestParams;
use rmcp::service::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

/// Write a source file under `root` and ensure `.derrick/` exists.
fn seed_repo(root: &Path, file: &str, contents: &str) {
    std::fs::write(root.join(file), contents).unwrap();
    std::fs::create_dir_all(root.join(".derrick")).unwrap();
}

/// A two-workspace hub with the given auth tokens. A defines `alpha_only_symbol`.
fn two_repos_with_auth(
    tokens: Vec<TokenConfig>,
) -> (tempfile::TempDir, tempfile::TempDir, HubConfig) {
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    seed_repo(repo_a.path(), "a.rs", "pub fn alpha_only_symbol() {}\n");
    seed_repo(repo_b.path(), "b.rs", "pub fn beta_only_symbol() {}\n");
    let config = HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        freshness_ttl_secs: 3600,
        auth: Some(AuthConfig { tokens }),
        workspaces: vec![
            WorkspaceConfig {
                id: "repo-a".to_owned(),
                root: Some(repo_a.path().to_path_buf()),
                db_path: None,
                pushed_db: None,
            },
            WorkspaceConfig {
                id: "repo-b".to_owned(),
                root: Some(repo_b.path().to_path_buf()),
                db_path: None,
                pushed_db: None,
            },
        ],
    };
    (repo_a, repo_b, config)
}

fn token(secret: &str, workspaces: &[&str], caps: &[Capability]) -> TokenConfig {
    TokenConfig {
        token: secret.to_owned(),
        workspaces: workspaces.iter().map(|s| s.to_string()).collect(),
        capabilities: caps.to_vec(),
    }
}

/// Spawn the auth-enabled hub on an ephemeral loopback port; returns the URI and
/// the server task handle.
async fn spawn_hub(config: &HubConfig) -> (String, tokio::task::JoinHandle<()>) {
    let hub = Hub::build(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    // Use the production wiring (build_router) so the test exercises the exact
    // middleware stack, including the bearer-auth layer.
    let app = build_router(hub, config);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/"), server)
}

/// Build a transport config carrying `token` (sent as
/// `Authorization: Bearer <token>`). The caller passes it to
/// `StreamableHttpClientTransport::from_config` so the (private) reqwest client
/// type is inferred rather than named — rmcp's reqwest version need not match
/// the workspace's.
fn auth_config(uri: &str, token: Option<&str>) -> StreamableHttpClientTransportConfig {
    let mut config = StreamableHttpClientTransportConfig::with_uri(uri);
    config.auth_header = token.map(|t| t.to_owned());
    config
}

/// Call `derrick_survey_search` for `workspace`/`query` and return the joined
/// text content (or the error debug string).
async fn call_search(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    workspace: &str,
    query: &str,
) -> Result<String, String> {
    let mut req = CallToolRequestParams::default();
    req.name = "derrick_survey_search".to_owned().into();
    req.arguments = serde_json::json!({ "workspace": workspace, "query": query })
        .as_object()
        .cloned();
    match client.call_tool(req).await {
        Ok(result) => Ok(result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()),
        Err(e) => Err(format!("{e:?}")),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_or_bad_token_is_rejected_and_valid_token_accepted() {
    let (_a, _b, config) =
        two_repos_with_auth(vec![token("good-secret", &["*"], &[Capability::Read])]);
    let (uri, server) = spawn_hub(&config).await;

    // No token: the HTTP middleware returns 401, so the MCP handshake fails.
    let no_token = ()
        .serve(StreamableHttpClientTransport::from_config(auth_config(
            &uri, None,
        )))
        .await;
    assert!(no_token.is_err(), "missing token must be rejected");

    // Bad token: same rejection.
    let bad = ()
        .serve(StreamableHttpClientTransport::from_config(auth_config(
            &uri,
            Some("nope"),
        )))
        .await;
    assert!(bad.is_err(), "unknown token must be rejected");

    // Valid token: the handshake succeeds and a read is served.
    let client = ()
        .serve(StreamableHttpClientTransport::from_config(auth_config(
            &uri,
            Some("good-secret"),
        )))
        .await
        .expect("valid token must be accepted");
    let out = call_search(&client, "repo-a", "alpha_only_symbol")
        .await
        .expect("valid read should succeed");
    assert!(out.contains("alpha_only_symbol"), "expected hit: {out}");

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_scoped_to_one_workspace_is_denied_others() {
    let (_a, _b, config) =
        two_repos_with_auth(vec![token("a-only", &["repo-a"], &[Capability::Read])]);
    let (uri, server) = spawn_hub(&config).await;

    let client = ()
        .serve(StreamableHttpClientTransport::from_config(auth_config(
            &uri,
            Some("a-only"),
        )))
        .await
        .unwrap();

    // Allowed workspace: succeeds.
    let in_a = call_search(&client, "repo-a", "alpha_only_symbol")
        .await
        .expect("repo-a is in scope");
    assert!(in_a.contains("alpha_only_symbol"), "expected hit: {in_a}");

    // Out-of-scope workspace: forbidden.
    let denied = call_search(&client, "repo-b", "beta_only_symbol")
        .await
        .expect_err("repo-b is out of scope");
    assert!(
        denied.contains("forbidden"),
        "expected a forbidden error: {denied}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_token_is_denied_refresh() {
    let (_a, _b, config) = two_repos_with_auth(vec![token("reader", &["*"], &[Capability::Read])]);
    let (uri, server) = spawn_hub(&config).await;

    let client = ()
        .serve(StreamableHttpClientTransport::from_config(auth_config(
            &uri,
            Some("reader"),
        )))
        .await
        .unwrap();

    // Read works.
    call_search(&client, "repo-a", "alpha_only_symbol")
        .await
        .expect("read is allowed");

    // Refresh requires the `refresh` capability the token lacks.
    let mut req = CallToolRequestParams::default();
    req.name = "derrick_survey_refresh".to_owned().into();
    req.arguments = serde_json::json!({ "workspace": "repo-a" })
        .as_object()
        .cloned();
    let denied = client.call_tool(req).await.expect_err("refresh forbidden");
    let msg = format!("{denied:?}");
    assert!(
        msg.contains("forbidden") && msg.to_lowercase().contains("capab"),
        "expected a capability-forbidden error: {msg}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcard_token_reaches_all_workspaces() {
    let (_a, _b, config) = two_repos_with_auth(vec![token(
        "super",
        &["*"],
        &[Capability::Read, Capability::Refresh],
    )]);
    let (uri, server) = spawn_hub(&config).await;

    let client = ()
        .serve(StreamableHttpClientTransport::from_config(auth_config(
            &uri,
            Some("super"),
        )))
        .await
        .unwrap();

    let in_a = call_search(&client, "repo-a", "alpha_only_symbol")
        .await
        .expect("repo-a allowed");
    assert!(in_a.contains("alpha_only_symbol"));

    let in_b = call_search(&client, "repo-b", "beta_only_symbol")
        .await
        .expect("repo-b allowed");
    assert!(in_b.contains("beta_only_symbol"));

    // Refresh is also permitted for the wildcard token.
    let mut req = CallToolRequestParams::default();
    req.name = "derrick_survey_refresh".to_owned().into();
    req.arguments = serde_json::json!({ "workspace": "repo-b" })
        .as_object()
        .cloned();
    client.call_tool(req).await.expect("refresh allowed");

    client.cancel().await.unwrap();
    server.abort();
}
