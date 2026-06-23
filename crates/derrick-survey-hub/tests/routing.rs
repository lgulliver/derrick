//! Multi-instance routing tests for the survey hub (D80, phase 1).
//!
//! The load-bearing assertion: a `workspace`-routed search against repo A finds
//! a symbol that only A defines, and the same search against repo B does not.
//! That proves the `workspace` argument selects the right [`Survey`] instance.
//!
//! Uses real temp repos and real SQLite (house rule: no mocks). The primary
//! test drives the hub over its real HTTP transport via an rmcp client; a
//! `Hub`-level test asserts the same routing without the network in case the
//! HTTP transport is awkward in CI.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use derrick_survey_hub::{Hub, HubConfig, HubServer, WorkspaceConfig, WorkspaceId};
use rmcp::model::CallToolRequestParams;
use rmcp::service::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

/// Write a source file under `root` and ensure `.derrick/` exists.
fn seed_repo(root: &Path, file: &str, contents: &str) {
    std::fs::write(root.join(file), contents).unwrap();
    std::fs::create_dir_all(root.join(".derrick")).unwrap();
}

/// Build a two-workspace config: A defines `alpha_only_symbol`, B does not.
fn two_repos() -> (tempfile::TempDir, tempfile::TempDir, HubConfig) {
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    seed_repo(
        repo_a.path(),
        "a.rs",
        "pub fn alpha_only_symbol() {}\npub fn shared() {}\n",
    );
    seed_repo(
        repo_b.path(),
        "b.rs",
        "pub fn beta_only_symbol() {}\npub fn shared() {}\n",
    );
    let config = HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        workspaces: vec![
            WorkspaceConfig {
                id: "repo-a".to_owned(),
                root: repo_a.path().to_path_buf(),
                db_path: None,
            },
            WorkspaceConfig {
                id: "repo-b".to_owned(),
                root: repo_b.path().to_path_buf(),
                db_path: None,
            },
        ],
    };
    (repo_a, repo_b, config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_level_search_is_routed_per_workspace() {
    let (_a, _b, config) = two_repos();
    let hub = Hub::build(&config).await.unwrap();

    let repo_a = WorkspaceId::new("repo-a").unwrap();
    let repo_b = WorkspaceId::new("repo-b").unwrap();

    let entry_a = hub.entry(&repo_a).await.unwrap();
    let entry_b = hub.entry(&repo_b).await.unwrap();

    // The symbol only exists in A.
    let in_a = entry_a
        .survey
        .search("alpha_only_symbol", 10)
        .await
        .unwrap();
    assert!(
        in_a.iter().any(|h| h.name == "alpha_only_symbol"),
        "repo-a should contain alpha_only_symbol: {in_a:?}"
    );
    let in_b = entry_b
        .survey
        .search("alpha_only_symbol", 10)
        .await
        .unwrap();
    assert!(
        !in_b.iter().any(|h| h.name == "alpha_only_symbol"),
        "repo-b must not contain alpha_only_symbol: {in_b:?}"
    );

    // Unknown workspace ids are simply absent from the map.
    assert!(
        hub.entry(&WorkspaceId::new("nope").unwrap())
            .await
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_search_is_routed_per_workspace() {
    let (_a, _b, mut config) = two_repos();
    let hub = Hub::build(&config).await.unwrap();

    // Bind an ephemeral loopback port, then serve the hub on it.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    config.bind = addr;

    let service = StreamableHttpService::new(
        move || Ok(HubServer::new(hub.clone())),
        std::sync::Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().fallback_service(service);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Connect an MCP client over the streamable-HTTP transport.
    let uri = format!("http://{addr}/");
    let transport = StreamableHttpClientTransport::from_uri(uri);
    let client = ().serve(transport).await.unwrap();

    // Routed to repo-a: finds the A-only symbol.
    let found_in_a = call_search(&client, "repo-a", "alpha_only_symbol").await;
    assert!(
        found_in_a.contains("alpha_only_symbol"),
        "routing to repo-a should find alpha_only_symbol: {found_in_a}"
    );

    // Routed to repo-b: the A-only symbol is absent (no name in the body).
    let found_in_b = call_search(&client, "repo-b", "alpha_only_symbol").await;
    assert!(
        !found_in_b.contains("alpha_only_symbol"),
        "routing to repo-b must not find alpha_only_symbol: {found_in_b}"
    );

    // An unknown workspace yields a clear error rather than empty results.
    let mut req = CallToolRequestParams::default();
    req.name = "derrick_survey_search".to_owned().into();
    req.arguments = serde_json::json!({ "workspace": "ghost", "query": "shared" })
        .as_object()
        .cloned();
    let err = client.call_tool(req).await;
    assert!(err.is_err(), "unknown workspace must error: {err:?}");

    client.cancel().await.unwrap();
    server.abort();
}

/// Call `derrick_survey_search` for `workspace`/`query`, returning the joined
/// text content.
async fn call_search(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    workspace: &str,
    query: &str,
) -> String {
    let mut req = CallToolRequestParams::default();
    req.name = "derrick_survey_search".to_owned().into();
    req.arguments = serde_json::json!({ "workspace": workspace, "query": query })
        .as_object()
        .cloned();
    let result = client.call_tool(req).await.unwrap();
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}
