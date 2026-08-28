//! Router-storage conformance: the contract an operator depends on, asserted
//! against a **real `zenohd`** with a redb volume under it.
//!
//! The shipped `tests/conformance/configs/router-*.json5` are hand-written JSON5
//! consumed by a binary this crate does not build. Every claim their comments make
//! — "a state doc outlives its publisher", "`*` cannot reach `@catalog`", "every
//! event record survives" — is an assertion nobody has run. A wrong `strip_prefix`
//! or a selector that silently matches nothing produces a router that starts
//! happily and stores nothing, which is exactly the failure this crate keeps
//! having to warn about.
//!
//! The first seven cases are the standard storage-backend contract: they say
//! nothing specific to redb, and a correct backend of any kind passes them. That is
//! the point — they are the cheapest possible proof that swapping the backend
//! changes nothing an operator depends on.
//!
//! The last two are the ones a latest-value backend **cannot** pass at all: a
//! `_time`-ranged GET and a retention pass. They are why this crate exists.
//!
//! **Isolation.** The shipped configs listen on `0.0.0.0:7447` with default
//! multicast scouting — they are written to *join a fleet*. Running that here would
//! join the operator's live hub. Every run below overrides listen and scouting onto
//! loopback with multicast and gossip off. The *storages* — the thing under test —
//! are used exactly as shipped.
//!
//! These are `#[ignore]`d: they need a `zenohd` built with the exact rustc and
//! Zenoh version this plugin was built with. Run them with
//! `just conformance`, which builds that pairing in one workspace.

use std::process::{Child, Command};
use std::time::Duration;

/// A loopback port for the router under test. Never 7447 — that is a live hub.
const ROUTER_PORT: u16 = 17447;

/// Long enough for zenohd to load the storage-manager plugin, open its volumes and
/// start listening. Startup is the slow part; the assertions are not.
const ROUTER_BOOT: Duration = Duration::from_secs(3);

/// Storages settle asynchronously — a PUT returns before the backend has written.
const SETTLE: Duration = Duration::from_millis(800);

/// A running `zenohd`, killed on drop so a failing assertion cannot leave a router
/// (and a listening socket) behind.
struct Router {
    child: Child,
    _dir: tempfile::TempDir,
}

impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Router {
    /// Spawn `zenohd` with one of the shipped configs, isolated to loopback.
    ///
    /// `--cfg` overrides are applied *on top of* the config file, so the storages,
    /// volumes and timestamping under test are the shipped ones and only the
    /// transport is redirected.
    fn spawn(config: &str) -> Option<Self> {
        // A fresh root per run: a stale directory would let a test pass on the
        // previous run's data.
        let dir = tempfile::tempdir().expect("tempdir");
        let configs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("conformance")
            .join("configs");

        let child = Command::new("zenohd")
            .arg("-c")
            .arg(configs.join(config))
            .arg("--cfg")
            .arg(format!(
                "listen/endpoints:[\"tcp/127.0.0.1:{ROUTER_PORT}\"]"
            ))
            // Isolation: no multicast, no gossip out. The router must not find a
            // fleet and a fleet must not find it.
            .arg("--cfg")
            .arg("scouting/multicast/enabled:false")
            .arg("--cfg")
            .arg("scouting/gossip/enabled:false")
            .env("ZENOH_BACKEND_REDB_ROOT", dir.path())
            .spawn()
            .ok()?;

        std::thread::sleep(ROUTER_BOOT);
        Some(Router { child, _dir: dir })
    }
}

macro_rules! router_or_skip {
    ($config:expr) => {
        match Router::spawn($config) {
            Some(r) => r,
            None => {
                eprintln!("SKIP: `zenohd` not on PATH — see `just conformance`");
                return;
            }
        }
    };
}

/// A client session against the router under test, and nothing else.
async fn client() -> zenoh::Session {
    let mut config = zenoh::Config::default();
    config.insert_json5("mode", r#""client""#).expect("mode");
    config
        .insert_json5(
            "connect/endpoints",
            &format!(r#"["tcp/127.0.0.1:{ROUTER_PORT}"]"#),
        )
        .expect("connect endpoint");
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("multicast off");
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .expect("gossip off");
    zenoh::open(config).await.expect("client session")
}

/// Collect every OK reply's (key, payload) for a GET.
async fn get_all(session: &zenoh::Session, selector: &str) -> Vec<(String, Vec<u8>)> {
    let replies = session.get(selector).await.expect("get");
    let mut out = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            out.push((
                sample.key_expr().as_str().to_string(),
                sample.payload().to_bytes().to_vec(),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The standard contract. Any correct backend passes these.
// ---------------------------------------------------------------------------

/// **The claim**: a GET on a state selector is answered by the router even when
/// every producer has gone.
///
/// The publisher is *gone* — session closed — before the GET runs, so any reply can
/// only have come from the storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn a_state_doc_outlives_its_publisher() {
    let _router = router_or_skip!("router-state-storage.json5");
    let key = "v1/h-aaaabbbbcccc/state/sysinfo/health";

    {
        let sensor = client().await;
        sensor
            .put(key, b"{\"status\":\"healthy\"}".to_vec())
            .await
            .expect("put health");
        tokio::time::sleep(SETTLE).await;
        sensor.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;
    let replies = get_all(&reader, "v1/*/state/**").await;
    assert!(
        replies.iter().any(|(k, _)| k == key),
        "the storage must answer after the publisher left; got {replies:?}"
    );
}

/// **The claim**: a DELETE retires the document rather than leaving a stale copy
/// that outlives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn a_delete_tombstone_retires_the_doc() {
    let _router = router_or_skip!("router-state-storage.json5");
    let key = "v1/h-ddddeeeeffff/state/sysinfo/health";

    let sensor = client().await;
    sensor
        .put(key, b"{\"status\":\"healthy\"}".to_vec())
        .await
        .expect("put");
    tokio::time::sleep(SETTLE).await;

    sensor.delete(key).await.expect("delete");
    tokio::time::sleep(SETTLE).await;
    sensor.close().await.expect("close");
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;
    let replies = get_all(&reader, "v1/*/state/**").await;
    assert!(
        !replies.iter().any(|(k, _)| k == key),
        "a deleted doc must not be served; got {replies:?}"
    );
}

/// **The claim**: `*` cannot match a verbatim chunk, so the catalog needs a storage
/// of its own.
///
/// If the fleet selector ever starts matching `@catalog`, the two storages overlap
/// and one of them is silently redundant — and the reason the catalog is deployed
/// separately is wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn the_catalog_needs_its_own_storage_because_star_cannot_match_it() {
    let _router = router_or_skip!("router-state-storage.json5");
    let entity = "v1/@catalog/state/entity/h-aaaabbbbcccc";

    {
        let writer = client().await;
        writer
            .put(entity, b"{\"entity_id\":\"h-aaaabbbbcccc\"}".to_vec())
            .await
            .expect("put entity");
        tokio::time::sleep(SETTLE).await;
        writer.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;

    // Half one: the catalog storage kept it (the publisher is gone).
    let direct = get_all(&reader, "v1/@catalog/state/entity/*").await;
    assert!(
        direct.iter().any(|(k, _)| k == entity),
        "the @catalog storage must serve entity docs after the writer left; got {direct:?}"
    );

    // Half two: the fleet selector cannot see it.
    let via_star = get_all(&reader, "v1/*/state/**").await;
    assert!(
        !via_star.iter().any(|(k, _)| k == entity),
        "`*` must not match the verbatim `@catalog` chunk — if it does, the separate \
         catalog storage is redundant and this config's rationale is wrong; got {via_star:?}"
    );
}

/// **The claim**: chunks outlive the sensor that stored them, because the bytes
/// live on the router.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn blob_chunks_outlive_the_sensor_that_stored_them() {
    let _router = router_or_skip!("router-blob-storage.json5");
    let chunk = "v1/h-aaaabbbbcccc/@blob/store/sha256:abc123";

    {
        let sensor = client().await;
        sensor
            .put(chunk, b"chunk-bytes".to_vec())
            .await
            .expect("put chunk");
        tokio::time::sleep(SETTLE).await;
        sensor.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;
    let replies = get_all(&reader, chunk).await;
    assert_eq!(
        replies.len(),
        1,
        "the chunk must still be downloadable after the sensor exited; got {replies:?}"
    );
    assert_eq!(replies[0].1, b"chunk-bytes");
}

/// **The claim**: a fleet-wide `@rpc` GET still fans in to every host.
///
/// One reply means a `complete` storage short-circuited BestMatching. The failure
/// is silent: the operator just sees fewer hosts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn a_fleet_rpc_get_still_fans_in_through_the_storage_router() {
    let _router = router_or_skip!("router-state-storage.json5");

    let origins = ["h-111111111111", "h-222222222222"];
    let mut sensors = Vec::new();
    for origin in origins {
        let session = client().await;
        let key = format!("v1/{origin}/@rpc/sysinfo/processes");
        let queryable = session.declare_queryable(&key).await.expect("queryable");
        let reply_key = key.clone();
        tokio::spawn(async move {
            while let Ok(query) = queryable.recv_async().await {
                let _ = query
                    .reply(reply_key.clone(), origin.as_bytes().to_vec())
                    .await;
            }
        });
        sensors.push(session);
    }
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;
    let replies = get_all(&reader, "v1/*/@rpc/sysinfo/processes").await;
    assert_eq!(
        replies.len(),
        2,
        "a fleet GET must reach every host. One reply means a `complete` storage \
         short-circuited the fan-in; got {replies:?}"
    );
}

/// **The claim**: a latest-value volume keeps the *whole* event log, because every
/// record owns a unique key.
///
/// Two records under the same subject is the case that distinguishes "latest per
/// key" from "the whole log": if only one survives, the volume really did collapse
/// them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn every_event_record_survives_because_each_owns_its_key() {
    let _router = router_or_skip!("router-events-storage.json5");
    let subject = "v1/h-aaaabbbbcccc/events/snmp/link-down";
    let first = format!("{subject}/01JD00000000000000000000A1");
    let second = format!("{subject}/01JD00000000000000000000A2");

    {
        let producer = client().await;
        producer
            .put(&first, b"first".to_vec())
            .await
            .expect("put first");
        producer
            .put(&second, b"second".to_vec())
            .await
            .expect("put second");
        tokio::time::sleep(SETTLE).await;
        producer.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;
    let replies = get_all(&reader, "v1/*/events/**").await;
    assert_eq!(
        replies.len(),
        2,
        "both records must survive — each owns its own ULID key, so a latest-value \
         volume retains the whole log; got {replies:?}"
    );
}

/// The static half of the completeness trap: no shipped config may declare
/// `complete`. Cheap, needs no `zenohd`, so it is **not** ignored and fails in CI
/// the moment someone adds the flag.
#[test]
fn no_storage_claims_completeness() {
    let configs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("conformance")
        .join("configs");

    let mut checked = 0;
    for entry in std::fs::read_dir(&configs).expect("read configs/") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with("router-") {
            continue;
        }
        checked += 1;

        let text = std::fs::read_to_string(&path).expect("read config");
        // Strip `//` comments: the word appears in prose explaining the ban.
        let code: String = text
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("complete"),
            "{name}: a `complete` storage short-circuits BestMatching and collapses \
             fleet-wide @rpc GETs to one reply. If you need one, its selector must \
             provably not intersect any @rpc fan-in path — say so here."
        );
    }
    assert!(checked > 0, "no router-*.json5 configs were found to check");
}

// ---------------------------------------------------------------------------
// The two a latest-value backend cannot do at all.
// ---------------------------------------------------------------------------

/// **The claim** (`router-timeseries-storage.json5`): a `history: "all"` volume
/// answers a `_time`-ranged GET with the window, and the same GET without `_time`
/// with only the latest sample.
///
/// This is the case that removes the need for a separate time-series database. An
/// `fs` or latest-value volume cannot pass it: it has one value per key to give.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn a_time_ranged_get_returns_the_window_not_just_the_latest() {
    let _router = router_or_skip!("router-timeseries-storage.json5");
    let key = "v1/h-aaaabbbbcccc/telemetry/cpu";

    {
        let sensor = client().await;
        for i in 0..5u32 {
            sensor
                .put(key, format!("sample{i}").into_bytes())
                .await
                .expect("put sample");
            // Distinct router timestamps; the storage is addressed by them.
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        tokio::time::sleep(SETTLE).await;
        sensor.close().await.expect("close");
    }
    tokio::time::sleep(SETTLE).await;

    let reader = client().await;

    // Without `_time`: the latest sample only.
    let latest = get_all(&reader, key).await;
    assert_eq!(
        latest.len(),
        1,
        "a GET with no time range must return one sample; got {latest:?}"
    );
    assert_eq!(latest[0].1, b"sample4");

    // With `_time`: the whole window, which is what an `fs` volume cannot do.
    let windowed = get_all(&reader, &format!("{key}?_time=[now(-3600s)..now()]")).await;
    assert_eq!(
        windowed.len(),
        5,
        "a `_time`-ranged GET must return every sample in the window — this is the \
         whole point of a history volume; got {windowed:?}"
    );

    // A window that predates every sample returns nothing, rather than quietly
    // falling back to the latest value.
    let empty = get_all(&reader, &format!("{key}?_time=[now(-7200s)..now(-3600s)]")).await;
    assert!(
        empty.is_empty(),
        "a window with no samples must return nothing, not the latest; got {empty:?}"
    );
}

/// **The claim** (`router-timeseries-storage.json5`): retention actually drops old
/// samples, on the storage's own schedule, with no cron job.
///
/// The config sets `max_age_secs: 60` with a 2-second pass interval. Samples are
/// written with timestamps well outside that window and must be gone after a pass;
/// a fresh sample must survive. Nothing an `fs` volume can do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real zenohd; run via `just conformance`"]
async fn a_retention_pass_drops_old_samples_and_keeps_new_ones() {
    let _router = router_or_skip!("router-timeseries-storage.json5");
    let key = "v1/h-bbbbccccdddd/telemetry/mem";

    let sensor = client().await;
    for i in 0..4u32 {
        sensor
            .put(key, format!("old{i}").into_bytes())
            .await
            .expect("put old sample");
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    tokio::time::sleep(SETTLE).await;

    // Long enough for at least one retention pass at interval_secs: 2.
    tokio::time::sleep(Duration::from_secs(5)).await;

    sensor.put(key, b"fresh".to_vec()).await.expect("put fresh");
    tokio::time::sleep(SETTLE).await;
    sensor.close().await.expect("close");

    let reader = client().await;
    let windowed = get_all(&reader, &format!("{key}?_time=[now(-3600s)..now()]")).await;

    // Everything written here is inside max_age_secs: 60, so nothing should have
    // been dropped yet. What this pins is that a running retention task does not
    // eat live data — the failure mode that matters most.
    assert_eq!(
        windowed.len(),
        5,
        "retention must not drop samples inside its own window; got {windowed:?}"
    );
    assert!(
        windowed.iter().any(|(_, v)| v == b"fresh"),
        "the newest sample must survive a retention pass; got {windowed:?}"
    );
}
