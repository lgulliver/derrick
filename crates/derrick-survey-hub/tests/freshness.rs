//! Phase 2 freshness tests for the survey hub (D80): poll-on-query TTL and the
//! explicit `derrick_survey_refresh` tool.
//!
//! The load-bearing assertions:
//! - With a zero TTL, a query re-probes and incrementally rebuilds, so a symbol
//!   added after startup is found on the next query.
//! - With a long TTL and no refresh, that same new symbol stays invisible —
//!   proving the TTL gate actually gates rather than rebuilding every time.
//! - `derrick_survey_refresh` forces a rebuild regardless of the TTL window.
//!
//! Real temp repos and real SQLite (house rule: no mocks). The refresh test is
//! driven over the real HTTP transport via an rmcp client, mirroring the
//! routing suite.

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

/// A single-workspace hub config over `root` with the given freshness TTL.
fn single_repo_config(root: &Path, ttl_secs: u64) -> HubConfig {
    HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        freshness_ttl_secs: ttl_secs,
        workspaces: vec![WorkspaceConfig {
            id: "repo".to_owned(),
            root: Some(root.to_path_buf()),
            db_path: None,
            pushed_db: None,
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_ttl_rebuilds_on_query() {
    let repo = tempfile::tempdir().unwrap();
    seed_repo(repo.path(), "a.rs", "pub fn original_symbol() {}\n");

    // Zero TTL: every query re-probes for staleness.
    let hub = Hub::build(&single_repo_config(repo.path(), 0))
        .await
        .unwrap();
    let id = WorkspaceId::new("repo").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    // First query: only the original symbol is indexed.
    entry.ensure_fresh(hub.freshness_ttl()).await.unwrap();
    let before = entry
        .survey()
        .await
        .search("late_symbol", 10)
        .await
        .unwrap();
    assert!(
        !before.iter().any(|h| h.name == "late_symbol"),
        "late_symbol should not exist before it is written: {before:?}"
    );

    // Write a NEW symbol file into the tree after startup.
    std::fs::write(repo.path().join("b.rs"), "pub fn late_symbol() {}\n").unwrap();

    // Next query re-probes (TTL 0), sees the pending file, and rebuilds.
    entry.ensure_fresh(hub.freshness_ttl()).await.unwrap();
    let after = entry
        .survey()
        .await
        .search("late_symbol", 10)
        .await
        .unwrap();
    assert!(
        after.iter().any(|h| h.name == "late_symbol"),
        "poll-on-query rebuild should surface late_symbol: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_ttl_gates_rebuild() {
    let repo = tempfile::tempdir().unwrap();
    seed_repo(repo.path(), "a.rs", "pub fn original_symbol() {}\n");

    // Long TTL: the connect-time build set last_checked, so within the window
    // no query re-probes — a later change must stay invisible.
    let hub = Hub::build(&single_repo_config(repo.path(), 3600))
        .await
        .unwrap();
    let id = WorkspaceId::new("repo").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    std::fs::write(repo.path().join("b.rs"), "pub fn gated_symbol() {}\n").unwrap();

    // Query inside the TTL window: the gate short-cuts, so no rebuild happens.
    entry.ensure_fresh(hub.freshness_ttl()).await.unwrap();
    let found = entry
        .survey()
        .await
        .search("gated_symbol", 10)
        .await
        .unwrap();
    assert!(
        !found.iter().any(|h| h.name == "gated_symbol"),
        "a long TTL must gate the rebuild, hiding gated_symbol: {found:?}"
    );

    // But an explicit refresh still reconciles immediately.
    let status = entry.force_refresh().await.unwrap();
    assert!(
        status.pending.is_empty(),
        "force_refresh should leave no pending files: {status:?}"
    );
    let after = entry
        .survey()
        .await
        .search("gated_symbol", 10)
        .await
        .unwrap();
    assert!(
        after.iter().any(|h| h.name == "gated_symbol"),
        "force_refresh should surface gated_symbol: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queries_are_single_flight_and_correct() {
    let repo = tempfile::tempdir().unwrap();
    seed_repo(repo.path(), "a.rs", "pub fn original_symbol() {}\n");

    let hub = Hub::build(&single_repo_config(repo.path(), 0))
        .await
        .unwrap();
    let id = WorkspaceId::new("repo").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    // A change lands, then many queries race to observe it. Exhaustively proving
    // "exactly one build ran" is hard; we assert no deadlock/panic and that every
    // racer sees the fresh data once the dust settles.
    std::fs::write(repo.path().join("b.rs"), "pub fn raced_symbol() {}\n").unwrap();

    let ttl = hub.freshness_ttl();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let entry = entry.clone();
        handles.push(tokio::spawn(async move {
            entry.ensure_fresh(ttl).await.unwrap();
            entry
                .survey()
                .await
                .search("raced_symbol", 10)
                .await
                .unwrap()
        }));
    }

    let mut any_found = false;
    for handle in handles {
        let hits = handle.await.unwrap();
        any_found |= hits.iter().any(|h| h.name == "raced_symbol");
    }
    // At least one racer ran after the rebuild; re-probe once more to be sure the
    // index converged regardless of interleaving.
    entry.ensure_fresh(ttl).await.unwrap();
    let final_hits = entry
        .survey()
        .await
        .search("raced_symbol", 10)
        .await
        .unwrap();
    assert!(
        any_found || final_hits.iter().any(|h| h.name == "raced_symbol"),
        "concurrent poll-on-query should converge on raced_symbol"
    );
    assert!(
        final_hits.iter().any(|h| h.name == "raced_symbol"),
        "after settling, raced_symbol must be indexed: {final_hits:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_refresh_tool_reindexes_workspace() {
    let repo = tempfile::tempdir().unwrap();
    seed_repo(repo.path(), "a.rs", "pub fn original_symbol() {}\n");

    // Long TTL so the only thing that can reindex is the explicit refresh tool.
    let mut config = single_repo_config(repo.path(), 3600);
    let hub = Hub::build(&config).await.unwrap();

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

    let uri = format!("http://{addr}/");
    let transport = StreamableHttpClientTransport::from_uri(uri);
    let client = ().serve(transport).await.unwrap();

    // Add a new symbol after startup; within the long TTL window a plain search
    // would not see it.
    std::fs::write(repo.path().join("b.rs"), "pub fn refreshed_symbol() {}\n").unwrap();

    // Force a rebuild via the refresh tool; its result reports zero pending.
    let refresh_text = call_tool(
        &client,
        "derrick_survey_refresh",
        serde_json::json!({ "workspace": "repo" }),
    )
    .await;
    assert!(
        refresh_text.contains("\"pending\": []") || refresh_text.contains("\"pending\":[]"),
        "refresh result should report no pending files: {refresh_text}"
    );

    // A subsequent search now finds the new symbol.
    let search_text = call_tool(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "workspace": "repo", "query": "refreshed_symbol" }),
    )
    .await;
    assert!(
        search_text.contains("refreshed_symbol"),
        "search after refresh should find refreshed_symbol: {search_text}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

/// Call `tool` with `arguments`, returning the joined text content.
async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
    arguments: serde_json::Value,
) -> String {
    let mut req = CallToolRequestParams::default();
    req.name = tool.to_owned().into();
    req.arguments = arguments.as_object().cloned();
    let result = client.call_tool(req).await.unwrap();
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}
