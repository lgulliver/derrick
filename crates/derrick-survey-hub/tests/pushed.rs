//! Pushed-workspace tests for the survey hub (D82).
//!
//! The load-bearing assertions:
//! - **Go/no-go:** a Pushed workspace serves a prebuilt `.db` (no `root`), and a
//!   routed search finds a symbol baked into that DB.
//! - **Hot-swap:** rebuilding the `.db` on disk (simulating a CI re-push) and
//!   then refreshing — or querying past a tiny TTL — surfaces a NEW symbol;
//!   under a long TTL with no refresh it stays hidden (the gate works).
//! - **Mixed config:** one Local + one Pushed workspace in the same hub, both
//!   queryable and routed to the right index.
//!
//! Real temp repos and real SQLite (house rule: no mocks). A prebuilt index is
//! produced exactly as CI would: open a normal `Survey` over a seeded repo,
//! build it into a staging `.db`, then atomically rename it into place and hand
//! that path to the hub as a Pushed source.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use derrick_survey::{BuildOptions, Survey, SurveyConfig};
use derrick_survey_hub::{
    Hub, HubConfig, HubServer, WorkspaceConfig, WorkspaceId, WorkspaceSource,
};
use rmcp::model::CallToolRequestParams;
use rmcp::service::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

/// Produce a prebuilt index `.db` at `db_path`, exactly as CI would push one:
/// build the index into a *fresh* temp file, fully close it (checkpointing WAL),
/// then atomically rename it over `db_path`.
///
/// The write-then-rename is load-bearing for the hot-swap semantics: overwriting
/// the served file's inode in place would let the hub's already-open SQLite
/// readers observe the new rows via WAL without a reload, defeating the freshness
/// gate. A rename installs a *new inode*, so the old handle keeps serving the old
/// index until the hub explicitly reopens — which is what real producers do
/// (write `index.db.new`, `mv` over `index.db`).
async fn build_prebuilt_db(db_path: &Path, source: &str) {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("lib.rs"), source).unwrap();
    // Build into a staging path in the same directory as the target so the
    // final rename stays on one filesystem (and is therefore atomic).
    let parent = db_path.parent().unwrap();
    let staging = parent.join(format!(
        "staging-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let survey = Survey::open(SurveyConfig {
            db_path: staging.clone(),
            repo_root: repo.path().to_path_buf(),
            reader_pool: SurveyConfig::DEFAULT_READER_POOL,
        })
        .await
        .unwrap();
        survey.build(BuildOptions::default()).await.unwrap();
        // Drop `survey` here so every connection closes before we move the files.
    }
    // The index runs in WAL mode, so committed rows may still live in the
    // `-wal` sidecar rather than the main `.db`. Move the whole set so the
    // installed index is consistent. Each rename installs a new inode, leaving
    // the hub's already-open handle (a different inode) serving the old index
    // until it explicitly reloads — mirroring a real producer's atomic push.
    for suffix in ["", "-wal", "-shm"] {
        let from = with_suffix(&staging, suffix);
        let to = with_suffix(db_path, suffix);
        match std::fs::rename(&from, &to) {
            Ok(()) => {}
            // A sidecar may not exist (e.g. checkpointed away on close); then
            // remove any stale target so it does not shadow the fresh main DB.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_file(&to);
            }
            Err(e) => panic!("install {}: {e}", from.display()),
        }
    }
}

/// Append a sidecar suffix (`-wal` / `-shm`) to a DB path's file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        return path.to_path_buf();
    }
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// A single Pushed-workspace hub config pointing at `db_path`.
fn pushed_config(db_path: &Path, ttl_secs: u64) -> HubConfig {
    HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        freshness_ttl_secs: ttl_secs,
        auth: None,
        workspaces: vec![WorkspaceConfig {
            id: "pushed".to_owned(),
            root: None,
            db_path: None,
            pushed_db: Some(db_path.to_path_buf()),
        }],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_workspace_serves_prebuilt_db() {
    // Build a prebuilt DB at a path with NO working tree alongside it.
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn pushed_symbol() {}\n").await;

    // A long TTL: serving is purely connect-time open, no rebuild from a root.
    let hub = Hub::build(&pushed_config(&db_path, 3600)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    assert!(
        matches!(entry.source(), WorkspaceSource::Pushed { .. }),
        "workspace should be Pushed"
    );

    let hits = entry
        .survey()
        .await
        .search("pushed_symbol", 10)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.name == "pushed_symbol"),
        "pushed workspace should serve the prebuilt symbol: {hits:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_status_is_fresh_not_everything_deleted() {
    // A pushed `.db` lives in a dir with NO working tree alongside it. The old
    // bug: status() walked that dir, found none of the indexed files, and
    // reported every file `deleted` + freshness `stale`. The source-aware
    // status path must instead report empty pending + a non-stale label.
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(
        &db_path,
        "pub fn one() {}\npub fn two() {}\npub fn three() {}\n",
    )
    .await;

    let hub = Hub::build(&pushed_config(&db_path, 3600)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    let status = entry.status().await.unwrap();
    assert!(
        status.pending.is_empty(),
        "a pushed index has no working tree to diff; pending must be empty \
         (the previous bogus 'everything deleted' is gone): {status:?}"
    );
    assert!(
        !status.freshness.starts_with("stale"),
        "a pushed index must not read stale: {status:?}"
    );
    assert_eq!(status.freshness, "fresh");
    assert_eq!(
        status.files, 1,
        "the prebuilt index covers one source file (lib.rs): {status:?}"
    );
    assert!(
        status.symbols >= 3,
        "the prebuilt index holds the three baked symbols: {status:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_force_refresh_returns_fresh_status() {
    // force_refresh (the `derrick_survey_refresh` return value) must come back
    // with the source-aware status: empty pending + fresh, not the old bogus
    // "everything deleted / stale".
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn refreshed_symbol() {}\n").await;

    let hub = Hub::build(&pushed_config(&db_path, 3600)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    let status = entry.force_refresh().await.unwrap();
    assert!(
        status.pending.is_empty(),
        "force_refresh on a pushed index must return empty pending: {status:?}"
    );
    assert!(
        !status.freshness.starts_with("stale"),
        "force_refresh on a pushed index must not read stale: {status:?}"
    );
    assert_eq!(status.freshness, "fresh");
    assert_eq!(status.files, 1, "one source file in the prebuilt index");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_force_refresh_hot_swaps_new_db() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn first_symbol() {}\n").await;

    // Long TTL so the only thing that can reload is the explicit refresh.
    let hub = Hub::build(&pushed_config(&db_path, 3600)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    // The original symbol is served; the future one is not yet present.
    let before = entry
        .survey()
        .await
        .search("second_symbol", 10)
        .await
        .unwrap();
    assert!(
        !before.iter().any(|h| h.name == "second_symbol"),
        "second_symbol must not exist before the re-push: {before:?}"
    );

    // CI re-pushes a new index: atomically swap a new .db into place with a
    // NEW symbol (write-then-rename, as a real producer does).
    build_prebuilt_db(
        &db_path,
        "pub fn first_symbol() {}\npub fn second_symbol() {}\n",
    )
    .await;

    // Force a reload+swap via the refresh path.
    let status = entry.force_refresh().await.unwrap();
    assert!(
        status.symbols >= 2,
        "reloaded index should hold the new symbol set: {status:?}"
    );

    let after = entry
        .survey()
        .await
        .search("second_symbol", 10)
        .await
        .unwrap();
    assert!(
        after.iter().any(|h| h.name == "second_symbol"),
        "hot-swap reload should surface second_symbol: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_long_ttl_gates_reload() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn before_repush() {}\n").await;

    // Long TTL: a re-push within the window must NOT be observed without a
    // refresh — the freshness gate must gate.
    let hub = Hub::build(&pushed_config(&db_path, 3600)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    build_prebuilt_db(
        &db_path,
        "pub fn before_repush() {}\npub fn gated_repush() {}\n",
    )
    .await;

    // A query inside the TTL window short-cuts the freshness probe: no reload.
    entry.ensure_fresh(hub.freshness_ttl()).await.unwrap();
    let gated = entry
        .survey()
        .await
        .search("gated_repush", 10)
        .await
        .unwrap();
    assert!(
        !gated.iter().any(|h| h.name == "gated_repush"),
        "a long TTL must gate the reload, hiding gated_repush: {gated:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_zero_ttl_reloads_on_query() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn before_repush() {}\n").await;

    // Zero TTL: every query re-stats the .db and reloads if it changed.
    let hub = Hub::build(&pushed_config(&db_path, 0)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    let entry = hub.entry(&id).await.unwrap();

    // Re-push with a new symbol. Sleep a beat so the mtime second advances even
    // on coarse-granularity filesystems (the stat compares len+mtime).
    tokio::time::sleep(Duration::from_millis(1100)).await;
    build_prebuilt_db(
        &db_path,
        "pub fn before_repush() {}\npub fn live_repush() {}\n",
    )
    .await;

    // A query past the (zero) TTL re-stats, sees the change, and reloads.
    entry.ensure_fresh(hub.freshness_ttl()).await.unwrap();
    let after = entry
        .survey()
        .await
        .search("live_repush", 10)
        .await
        .unwrap();
    assert!(
        after.iter().any(|h| h.name == "live_repush"),
        "zero-TTL query should reload and surface live_repush: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_local_and_pushed_route_correctly() {
    // Pushed workspace: a prebuilt DB with a Pushed-only symbol.
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn pushed_only_symbol() {}\n").await;

    // Local workspace: a live working tree with a Local-only symbol.
    let local_repo = tempfile::tempdir().unwrap();
    std::fs::write(
        local_repo.path().join("a.rs"),
        "pub fn local_only_symbol() {}\n",
    )
    .unwrap();
    std::fs::create_dir_all(local_repo.path().join(".derrick")).unwrap();

    let config = HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        freshness_ttl_secs: 3600,
        auth: None,
        workspaces: vec![
            WorkspaceConfig {
                id: "local".to_owned(),
                root: Some(local_repo.path().to_path_buf()),
                db_path: None,
                pushed_db: None,
            },
            WorkspaceConfig {
                id: "pushed".to_owned(),
                root: None,
                db_path: None,
                pushed_db: Some(db_path.clone()),
            },
        ],
    };

    let hub = Hub::build(&config).await.unwrap();

    let local = hub
        .entry(&WorkspaceId::new("local").unwrap())
        .await
        .unwrap();
    let pushed = hub
        .entry(&WorkspaceId::new("pushed").unwrap())
        .await
        .unwrap();
    assert!(matches!(local.source(), WorkspaceSource::Local { .. }));
    assert!(matches!(pushed.source(), WorkspaceSource::Pushed { .. }));

    // The Local-only symbol is in Local and not in Pushed.
    let local_hits = local
        .survey()
        .await
        .search("local_only_symbol", 10)
        .await
        .unwrap();
    assert!(
        local_hits.iter().any(|h| h.name == "local_only_symbol"),
        "local workspace should contain local_only_symbol: {local_hits:?}"
    );
    let pushed_lacks_local = pushed
        .survey()
        .await
        .search("local_only_symbol", 10)
        .await
        .unwrap();
    assert!(
        !pushed_lacks_local
            .iter()
            .any(|h| h.name == "local_only_symbol"),
        "pushed workspace must not contain local_only_symbol: {pushed_lacks_local:?}"
    );

    // The Pushed-only symbol is in Pushed and not in Local.
    let pushed_hits = pushed
        .survey()
        .await
        .search("pushed_only_symbol", 10)
        .await
        .unwrap();
    assert!(
        pushed_hits.iter().any(|h| h.name == "pushed_only_symbol"),
        "pushed workspace should contain pushed_only_symbol: {pushed_hits:?}"
    );
    let local_lacks_pushed = local
        .survey()
        .await
        .search("pushed_only_symbol", 10)
        .await
        .unwrap();
    assert!(
        !local_lacks_pushed
            .iter()
            .any(|h| h.name == "pushed_only_symbol"),
        "local workspace must not contain pushed_only_symbol: {local_lacks_pushed:?}"
    );
}

/// Serve `hub` over an ephemeral loopback HTTP listener and connect an rmcp
/// client. Returns the client and the server task (abort it when done).
async fn serve_and_connect(
    hub: Hub,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let service = StreamableHttpService::new(
        move || Ok(HubServer::new(hub.clone())),
        std::sync::Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().fallback_service(service);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/"));
    let client = ().serve(transport).await.unwrap();
    (client, server)
}

/// Call `tool` with `args` and return the joined text content.
async fn call_text(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
    args: serde_json::Value,
) -> String {
    let mut req = CallToolRequestParams::default();
    req.name = tool.to_owned().into();
    req.arguments = args.as_object().cloned();
    let result = client.call_tool(req).await.unwrap();
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect()
}

/// A Pushed workspace must NOT emit a staleness banner even when its `dirty`
/// flag is armed (as it is during/around a `reload_pushed` window). The banner
/// is a tree-vs-index diff, and a pushed index has no working tree, so the diff
/// would bogusly report every indexed file as differing. We arm `dirty` on the
/// shared entry (the same `Arc<AtomicBool>` the request path observes) and prove
/// search/status come back with the payload but no `STALE:` text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_dirty_does_not_emit_stale_banner() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("prebuilt.db");
    build_prebuilt_db(&db_path, "pub fn pushed_symbol() {}\n").await;

    // Long TTL so ensure_fresh's gate short-cuts: it never reloads and never
    // clears the dirty flag we arm below.
    let hub = Hub::build(&pushed_config(&db_path, 3600)).await.unwrap();
    let id = WorkspaceId::new("pushed").unwrap();
    // Arm dirty on the shared entry; the request path observes the same Arc.
    let entry = hub.entry(&id).await.unwrap();
    entry.dirty.store(true, Ordering::Relaxed);

    let (client, server) = serve_and_connect(hub).await;

    let search = call_text(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "workspace": "pushed", "query": "pushed_symbol" }),
    )
    .await;
    assert!(
        !search.contains("STALE:"),
        "a dirty pushed workspace must not emit a tree-vs-index banner: {search}"
    );
    assert!(
        search.contains("pushed_symbol"),
        "the search payload is still served: {search}"
    );

    let status = call_text(
        &client,
        "derrick_survey_status",
        serde_json::json!({ "workspace": "pushed" }),
    )
    .await;
    assert!(
        !status.contains("STALE:"),
        "a dirty pushed workspace status must not emit a banner: {status}"
    );
    assert!(
        status.contains("\"freshness\""),
        "the status payload is still served: {status}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

/// A Local workspace must STILL emit the staleness banner when dirty + a
/// divergent tree, proving the source-aware change leaves Local (and therefore
/// the stdio server, which always passes `TreeBacked`) behaviour unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_dirty_and_pending_still_emits_stale_banner() {
    let local_repo = tempfile::tempdir().unwrap();
    std::fs::write(local_repo.path().join("a.rs"), "pub fn local_symbol() {}\n").unwrap();
    std::fs::create_dir_all(local_repo.path().join(".derrick")).unwrap();

    // Long TTL so ensure_fresh does not rebuild mid-test and clear dirty.
    let config = HubConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        freshness_ttl_secs: 3600,
        auth: None,
        workspaces: vec![WorkspaceConfig {
            id: "local".to_owned(),
            root: Some(local_repo.path().to_path_buf()),
            db_path: None,
            pushed_db: None,
        }],
    };
    let hub = Hub::build(&config).await.unwrap();
    let id = WorkspaceId::new("local").unwrap();

    // Diverge the tree from the index (an unindexed file) so `pending` is
    // non-empty, then arm dirty on the shared entry to arm the banner path.
    std::fs::write(
        local_repo.path().join("extra.rs"),
        "pub fn extra_symbol() {}\n",
    )
    .unwrap();
    let entry = hub.entry(&id).await.unwrap();
    entry.dirty.store(true, Ordering::Relaxed);

    let (client, server) = serve_and_connect(hub).await;

    let search = call_text(
        &client,
        "derrick_survey_search",
        serde_json::json!({ "workspace": "local", "query": "local_symbol" }),
    )
    .await;
    assert!(
        search.contains("STALE:"),
        "a dirty local workspace with a divergent tree must emit the banner: {search}"
    );
    assert!(
        search.contains("extra.rs"),
        "the banner should name the pending file: {search}"
    );
    assert!(
        search.contains("local_symbol"),
        "the search payload is still served after the banner: {search}"
    );

    client.cancel().await.unwrap();
    server.abort();
}

/// A Pushed workspace pointed at a missing `.db` fails to build with a clear
/// workspace error (rather than silently serving an empty index).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pushed_missing_db_is_a_workspace_error() {
    let missing = PathBuf::from("/nonexistent/derrick/does-not-exist.db");
    let result = Hub::build(&pushed_config(&missing, 3600)).await;
    let err = match result {
        Ok(_) => panic!("a missing pushed db must fail to open"),
        Err(err) => err,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("pushed"),
        "error should name the failing workspace: {msg}"
    );
}
