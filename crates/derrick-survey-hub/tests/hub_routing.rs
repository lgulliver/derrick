//! Hub routing additions (D84): the `derrick_survey_list_workspaces` discovery
//! tool and `/w/<id>` path-prefix routing, layered on the existing explicit
//! `workspace`-argument scheme.
//!
//! These drive the real rmcp streamable-HTTP transport (mirroring
//! `tests/routing.rs` and `tests/auth.rs`) against hubs built from real temp
//! repos and real SQLite (house rule: no mocks). Each test exercises the
//! production wiring via [`build_router`] so the exact mount + middleware stack
//! is covered.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use derrick_survey_hub::{
    AuthConfig, Capability, Hub, HubConfig, TokenConfig, WorkspaceConfig, build_router,
};
use rmcp::model::CallToolRequestParams;
use rmcp::service::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::sync::OwnedMutexGuard;

/// Write a source file under `root` and ensure `.derrick/` exists.
fn seed_repo(root: &Path, file: &str, contents: &str) {
    std::fs::write(root.join(file), contents).unwrap();
    std::fs::create_dir_all(root.join(".derrick")).unwrap();
}

/// A two-workspace hub config (A defines `alpha_only_symbol`, B defines
/// `beta_only_symbol`) with the given auth tokens.
fn two_repos(
    tokens: Option<Vec<TokenConfig>>,
) -> (tempfile::TempDir, tempfile::TempDir, HubConfig) {
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    seed_repo(repo_a.path(), "a.rs", "pub fn alpha_only_symbol() {}\n");
    seed_repo(repo_b.path(), "b.rs", "pub fn beta_only_symbol() {}\n");
    let config = HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        freshness_ttl_secs: 3600,
        auth: tokens.map(|tokens| AuthConfig { tokens }),
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

/// Spawn the hub on an ephemeral loopback port via the production `build_router`,
/// returning the base URI (no trailing slash) and the server task handle.
fn routing_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

async fn spawn_hub(
    config: &HubConfig,
) -> (String, tokio::task::JoinHandle<()>, OwnedMutexGuard<()>) {
    let guard = routing_test_lock().lock_owned().await;
    let hub = Hub::build(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(hub, config).expect("valid hub config builds a router");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), server, guard)
}

/// Connect an rmcp client to `uri` (optionally with a bearer token).
async fn connect(
    uri: &str,
    token: Option<&str>,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(uri.to_owned());
    cfg.auth_header = token.map(|t| t.to_owned());
    ().serve(StreamableHttpClientTransport::from_config(cfg))
        .await
        .expect("client should connect")
}

/// Call a tool with the given JSON arguments, returning the joined text content
/// on success or the error debug string on failure.
async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
    args: serde_json::Value,
) -> Result<String, String> {
    let mut req = CallToolRequestParams::default();
    req.name = tool.to_owned().into();
    req.arguments = args.as_object().cloned();
    match client.call_tool(req).await {
        Ok(result) => Ok(result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()),
        Err(e) => Err(format!("{e:?}")),
    }
}

// ---------------------------------------------------------------------------
// Discovery tool
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_lists_all_ids_without_auth() {
    let (_a, _b, config) = two_repos(None);
    let (uri, server, _guard) = spawn_hub(&config).await;
    let client = connect(&uri, None).await;

    let listing = call_tool(
        &client,
        "derrick_survey_list_workspaces",
        serde_json::json!({}),
    )
    .await
    .expect("listing should succeed");
    let ids: Vec<String> = serde_json::from_str(&listing).expect("listing is a JSON array");
    assert_eq!(
        ids,
        vec!["repo-a".to_owned(), "repo-b".to_owned()],
        "{listing}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_wildcard_token_sees_all() {
    let (_a, _b, config) = two_repos(Some(vec![token("super", &["*"], &[Capability::Read])]));
    let (uri, server, _guard) = spawn_hub(&config).await;
    let client = connect(&uri, Some("super")).await;

    let listing = call_tool(
        &client,
        "derrick_survey_list_workspaces",
        serde_json::json!({}),
    )
    .await
    .expect("listing should succeed");
    let ids: Vec<String> = serde_json::from_str(&listing).unwrap();
    assert_eq!(
        ids,
        vec!["repo-a".to_owned(), "repo-b".to_owned()],
        "{listing}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_scoped_token_sees_only_its_subset() {
    let (_a, _b, config) = two_repos(Some(vec![token(
        "a-only",
        &["repo-a"],
        &[Capability::Read],
    )]));
    let (uri, server, _guard) = spawn_hub(&config).await;
    let client = connect(&uri, Some("a-only")).await;

    let listing = call_tool(
        &client,
        "derrick_survey_list_workspaces",
        serde_json::json!({}),
    )
    .await
    .expect("listing should succeed");
    let ids: Vec<String> = serde_json::from_str(&listing).unwrap();
    assert_eq!(
        ids,
        vec!["repo-a".to_owned()],
        "scoped token sees only its own: {listing}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_on_pinned_mount_lists_just_that_id() {
    let (_a, _b, config) = two_repos(None);
    let (uri, server, _guard) = spawn_hub(&config).await;
    // Address the pinned /w/repo-b mount.
    let client = connect(&format!("{uri}/w/repo-b"), None).await;

    let listing = call_tool(
        &client,
        "derrick_survey_list_workspaces",
        serde_json::json!({}),
    )
    .await
    .expect("listing should succeed");
    let ids: Vec<String> = serde_json::from_str(&listing).unwrap();
    assert_eq!(
        ids,
        vec!["repo-b".to_owned()],
        "pinned mount lists only its id: {listing}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

// ---------------------------------------------------------------------------
// Path-prefix routing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_mount_resolves_without_workspace_argument() {
    let (_a, _b, config) = two_repos(None);
    let (uri, server, _guard) = spawn_hub(&config).await;

    // /w/repo-a, no `workspace` argument: resolves to repo-a and finds its symbol.
    let client_a = connect(&format!("{uri}/w/repo-a"), None).await;
    let in_a = call_tool(
        &client_a,
        "derrick_survey_search",
        serde_json::json!({ "query": "alpha_only_symbol" }),
    )
    .await
    .expect("search on /w/repo-a should succeed");
    assert!(
        in_a.contains("alpha_only_symbol"),
        "/w/repo-a should find its symbol: {in_a}"
    );
    client_a.cancel().await.unwrap();

    // /w/repo-b resolves to repo-b: the A-only symbol is absent.
    let client_b = connect(&format!("{uri}/w/repo-b"), None).await;
    let in_b = call_tool(
        &client_b,
        "derrick_survey_search",
        serde_json::json!({ "query": "alpha_only_symbol" }),
    )
    .await
    .expect("search on /w/repo-b should succeed");
    assert!(
        !in_b.contains("alpha_only_symbol"),
        "/w/repo-b must not find repo-a's symbol: {in_b}"
    );
    // And it does find its own symbol.
    let own_b = call_tool(
        &client_b,
        "derrick_survey_search",
        serde_json::json!({ "query": "beta_only_symbol" }),
    )
    .await
    .expect("search on /w/repo-b should succeed");
    assert!(
        own_b.contains("beta_only_symbol"),
        "/w/repo-b should find its own symbol: {own_b}"
    );
    client_b.cancel().await.unwrap();

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_mount_rejects_mismatched_workspace_argument() {
    let (_a, _b, config) = two_repos(None);
    let (uri, server, _guard) = spawn_hub(&config).await;
    let client = connect(&format!("{uri}/w/repo-a"), None).await;

    // A `workspace` argument that disagrees with the pin is an error, not a
    // silent cross-workspace query.
    let err = call_tool(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "workspace": "repo-b", "query": "alpha_only_symbol" }),
    )
    .await
    .expect_err("mismatched workspace argument must error");
    assert!(
        err.contains("does not match") && err.contains("repo-b"),
        "error should name the mismatch: {err}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

// ---------------------------------------------------------------------------
// Back-compat (root endpoint, explicit `workspace`)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_endpoint_with_explicit_workspace_still_works() {
    let (_a, _b, config) = two_repos(None);
    let (uri, server, _guard) = spawn_hub(&config).await;
    let client = connect(&uri, None).await;

    let in_a = call_tool(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "workspace": "repo-a", "query": "alpha_only_symbol" }),
    )
    .await
    .expect("explicit workspace on root should succeed");
    assert!(in_a.contains("alpha_only_symbol"), "{in_a}");

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_endpoint_without_workspace_errors_clearly() {
    let (_a, _b, config) = two_repos(None);
    let (uri, server, _guard) = spawn_hub(&config).await;
    let client = connect(&uri, None).await;

    let err = call_tool(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "query": "alpha_only_symbol" }),
    )
    .await
    .expect_err("root endpoint without workspace must error");
    assert!(
        err.contains("workspace required") && err.contains("/w/"),
        "error should explain how to address a workspace: {err}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

// ---------------------------------------------------------------------------
// Auth × path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_scoped_to_repo_a_is_forbidden_at_pinned_repo_b() {
    let (_a, _b, config) = two_repos(Some(vec![token(
        "a-only",
        &["repo-a"],
        &[Capability::Read],
    )]));
    let (uri, server, _guard) = spawn_hub(&config).await;
    // The token authenticates (it is a valid token), so the handshake succeeds;
    // the per-workspace authz must then forbid the resolved repo-b.
    let client = connect(&format!("{uri}/w/repo-b"), Some("a-only")).await;

    let denied = call_tool(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "query": "beta_only_symbol" }),
    )
    .await
    .expect_err("repo-b is out of the token's scope");
    assert!(
        denied.contains("forbidden"),
        "expected a forbidden error: {denied}"
    );

    client.cancel().await.unwrap();
    server.abort();
}
