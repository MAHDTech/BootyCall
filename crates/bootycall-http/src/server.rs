use axum::{
    Json, Router,
    extract::{ConnectInfo, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bootycall_log::{error, info};
use minijinja::context;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bootycall_core::config::{Config, HostConfig, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};

#[derive(RustEmbed)]
#[folder = "src/assets/"]
struct Asset;

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<parking_lot::RwLock<Config>>,
    pub state_store: StateStore,
    pub jinja_env: Arc<minijinja::Environment<'static>>,
}

#[derive(Deserialize)]
pub struct WallpaperQuery {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub hd_video: Option<String>,
}

#[derive(Deserialize)]
pub struct OverrideRequest {
    pub mac: String,
    pub target: String,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub hosts: Vec<bootycall_core::state::HostState>,
    pub configs: Vec<HostConfig>,
}

/// Shared kernel/initrd/boot payload for the poll_handler. Two sites in
/// poll_handler used to format almost-identical strings by hand — one for
/// "host is directly configured", one for "host got a manual override
/// target". Both go through this template now so a change to the shape of
/// the boot line (extra kernel arg, rewritten cache path, whatever) is
/// one edit instead of two coordinated ones.
///
/// The `target_of` variable is `None` in the direct-config case and
/// `Some("<original mac>")` in the override case, so the "Booting …"
/// echo line still communicates why this target is being served.
const BOOT_TEMPLATE: &str = r#"#!ipxe
{% if target_of %}echo Booting target {{ name }} for host {{ target_of }}...
{% else %}echo Booting {{ name }}...
{% endif %}kernel http://{{ server_ip_port }}/cache/{{ mac }}/kernel {{ cmdline }}
initrd http://{{ server_ip_port }}/cache/{{ mac }}/initrd
boot
"#;

const MENU_TEMPLATE: &str = r#"#!ipxe

# Load wallpaper if available
chain --timeout 5000 http://{{ server_ip_port }}/dynamic/wallpaper.ipxe || echo Failed to load dynamic wallpaper

:menu
menu BootyCall Network Boot Menu
{% for host in hosts %}
item host_{{ loop.index }} {{ host.name }}
{% endfor %}
item shell Enter iPXE shell
item reboot Reboot system

choose --default host_1 --timeout 30000 target || goto menu

# Handle targets
{% for host in hosts %}
:host_{{ loop.index }}
echo Booting {{ host.name }}...
kernel http://{{ server_ip_port }}/cache/{{ host.mac }}/kernel {{ host.cmdline | default('') }}
initrd http://{{ server_ip_port }}/cache/{{ host.mac }}/initrd
boot
{% endfor %}

:shell
shell
goto menu

:reboot
reboot
"#;

/// Map a path's extension to a MIME type. Shared by the on-disk file server
/// (`serve_file_from_dir`) and the embedded-asset server (`serve_asset`) so both
/// agree on content types — embedded `.png`/`.json` assets used to fall through
/// to `application/octet-stream`.
fn content_type_for(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_lowercase().as_str() {
        "ipxe" | "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "json" => "application/json",
        "css" => "text/css",
        "js" => "application/javascript",
        "html" => "text/html",
        // EFI bootloader binaries ("efi") and anything unrecognised.
        _ => "application/octet-stream",
    }
}

/// Run [`bootycall_core::safe_join`] on tokio's blocking thread pool.
///
/// CONVENTION (issue 070): handlers here are `async fn`s executing on tokio
/// worker threads and must never perform blocking `std::fs` work — stat,
/// canonicalize, open, read — directly, nor call helpers that do (such as
/// `safe_join`, which canonicalises twice). A blocked worker stalls unrelated
/// DHCP/TFTP/HTTP futures under load. Route all filesystem work through
/// `tokio::fs` (which offloads internally) or `tokio::task::spawn_blocking`,
/// as here.
///
/// A failed join (panicked or cancelled task) maps to `None`, i.e. deny —
/// never fail open on a path-containment check.
async fn safe_join_blocking(root: PathBuf, requested: String) -> Option<PathBuf> {
    match tokio::task::spawn_blocking(move || bootycall_core::safe_join(&root, &requested)).await {
        Ok(joined) => joined,
        Err(e) => {
            error!("safe_join task panicked or was cancelled: {e:?}");
            None
        }
    }
}

async fn serve_file_from_dir(dir: PathBuf, relative_path: String) -> Result<Response, StatusCode> {
    let full_path = safe_join_blocking(dir, relative_path.clone())
        .await
        .ok_or(StatusCode::FORBIDDEN)?;

    // One offloaded stat (tokio::fs runs it on the blocking pool) replaces the
    // old on-runtime `exists()` + `is_file()` pair; a missing path and a
    // non-file (directory, socket, …) both map to 404 as before.
    match tokio::fs::metadata(&full_path).await {
        Ok(meta) if meta.is_file() => {}
        _ => return Err(StatusCode::NOT_FOUND),
    }

    let file = match tokio::fs::File::open(&full_path).await {
        Ok(f) => f,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let content_type = content_type_for(&relative_path);

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    Ok(([(header::CONTENT_TYPE, content_type)], body).into_response())
}

async fn serve_asset(path: &str) -> Response {
    match Asset::get(path) {
        Some(content) => (
            [(header::CONTENT_TYPE, content_type_for(path))],
            content.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Handler for embedded UI routes
async fn ui_handler(AxumPath(path): AxumPath<String>) -> impl IntoResponse {
    serve_asset(&path).await
}

// Redirect root route to ui index
async fn root_redirect() -> impl IntoResponse {
    (
        [(header::LOCATION, "/ui/index.html")],
        StatusCode::SEE_OTHER,
    )
        .into_response()
}

// Serving static wallpaper files from the configured static dir
async fn serve_static_file(
    State(state): State<ServerState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let static_dir = {
        let config_guard = state.config.read();
        config_guard.server.static_dir.clone()
    };
    serve_file_from_dir(static_dir, path).await
}

// Serving cached kernels and initrds
async fn serve_cache_file(
    State(state): State<ServerState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let cache_dir = {
        let config_guard = state.config.read();
        config_guard.server.cache_dir.clone()
    };
    serve_file_from_dir(cache_dir, path).await
}

/// Strip an optional `:port` suffix from a `Host` header value, handling
/// bracketed IPv6 literals (`[::1]:8080` → `::1`). Unbracketed values with
/// more than one colon are returned untouched — an unbracketed IPv6 literal
/// has no unambiguous port separator.
fn host_without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.split_once(':') {
        Some((name, port))
            if !port.is_empty()
                && !port.contains(':')
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    }
}

/// Resolve the `host[:port]` clients are sent to in generated iPXE
/// boot-script URLs (bound as `server_ip_port` in the templates).
///
/// Reflecting the client-supplied `Host:` header verbatim lets an attacker
/// behind a path-keyed caching proxy poison the cached boot script so *other*
/// PXE clients fetch their kernel/initrd from an attacker host (issue 069),
/// so trusted configuration wins:
///
/// 1. `server.advertised_host`, when set, is always used; the `Host` header
///    is ignored entirely.
/// 2. Otherwise, when `server.allowed_hosts` is non-empty, the header is
///    reflected only if its host part (port stripped) matches an allowlist
///    entry; anything else is replaced by the first allowed host plus the
///    trusted `http_bind` port.
/// 3. With neither option configured the raw header is reflected as before
///    (falling back to the historical `localhost:8080` dev default) so
///    existing deployments keep booting — set `advertised_host` on any
///    installation fronted by a shared cache.
fn advertised_host_port(server: &ServerConfig, headers: &HeaderMap) -> String {
    if let Some(advertised) = server.advertised_host.as_deref() {
        return advertised.to_string();
    }

    let host_hdr = headers.get(header::HOST).and_then(|h| h.to_str().ok());

    if server.allowed_hosts.is_empty() {
        return host_hdr.unwrap_or("localhost:8080").to_string();
    }

    if let Some(hdr) = host_hdr {
        let host_only = host_without_port(hdr);
        if server
            .allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host_only))
        {
            return hdr.to_string();
        }
    }

    // Unknown or missing Host: replace it with the first allowed host and the
    // port from `http_bind` — the port carried by an attacker-supplied header
    // is just as untrusted as its hostname.
    let port = server
        .http_bind
        .rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or("8080");
    let host = server
        .allowed_hosts
        .first()
        .map(String::as_str)
        .unwrap_or("localhost");
    if host.contains(':') {
        // Bare IPv6 literal — bracket it so the port stays unambiguous.
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

// /start endpoint - chains client to MAC specific poll endpoint
async fn start_handler(State(state): State<ServerState>, headers: HeaderMap) -> impl IntoResponse {
    let server_host = {
        let config_guard = state.config.read();
        advertised_host_port(&config_guard.server, &headers)
    };

    let script = format!(
        "#!ipxe\necho BootyCall starting...\nchain --autofree --replace http://{}/poll/${{mac:hexhyp}}\n",
        server_host
    );

    ([(header::CONTENT_TYPE, "text/plain")], script)
}

/// What the `/poll/{mac}` endpoint decided to do for a client, separated from
/// the side effects (state updates, events, response building) so the decision
/// logic — including the render-failure branch — is unit-testable.
enum PollOutcome {
    /// Serve the rendered boot script; `cache_mac` is the MAC whose cached
    /// kernel/initrd the script points at.
    Boot { script: String, cache_mac: String },
    /// The boot template failed to render — surface an error, do not boot.
    RenderFailed,
    /// No boot target yet — keep the client polling.
    Poll,
}

/// Decide what to serve for a `/poll/{mac}` request: boot a directly-configured
/// host, boot a manually-overridden target, report a render failure, or keep
/// polling. Pure (no state mutation) so every branch can be tested directly.
/// `server_host` must already be resolved via [`advertised_host_port`] — never
/// pass the raw `Host:` header here.
fn decide_poll_outcome(
    boot_template: &minijinja::Template<'_, '_>,
    config: &Config,
    state_store: &StateStore,
    mac_str: &str,
    server_host: &str,
) -> PollOutcome {
    // 1. Host configured directly in yaml.
    if let Some(host_config) = config.find_host(mac_str) {
        return match boot_template.render(context! {
            name => host_config.name.as_str(),
            server_ip_port => server_host,
            mac => host_config.mac.as_str(),
            cmdline => host_config.cmdline.as_deref().unwrap_or(""),
            target_of => None::<&str>,
        }) {
            Ok(script) => PollOutcome::Boot {
                script,
                cache_mac: host_config.mac.clone(),
            },
            Err(e) => {
                error!("Failed to render boot script for host {}: {:?}", mac_str, e);
                PollOutcome::RenderFailed
            }
        };
    }

    // 2. Host registered with a manual override target that exists in config.
    if let Some(hs) = state_store.get_host(mac_str)
        && let Some(target_name) = hs.assigned_target.as_ref()
        && let Some(target_config) = config.hosts.iter().find(|h| &h.name == target_name)
    {
        return match boot_template.render(context! {
            name => target_config.name.as_str(),
            server_ip_port => server_host,
            mac => target_config.mac.as_str(),
            cmdline => target_config.cmdline.as_deref().unwrap_or(""),
            target_of => Some(mac_str),
        }) {
            Ok(script) => PollOutcome::Boot {
                script,
                cache_mac: target_config.mac.clone(),
            },
            Err(e) => {
                error!(
                    "Failed to render boot script for override target {}: {:?}",
                    mac_str, e
                );
                PollOutcome::RenderFailed
            }
        };
    }

    // 3. Nothing to boot yet — keep polling.
    PollOutcome::Poll
}

// /poll/{mac} endpoint
async fn poll_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    AxumPath(mac): AxumPath<String>,
) -> impl IntoResponse {
    let server_host = {
        let config_guard = state.config.read();
        advertised_host_port(&config_guard.server, &headers)
    };

    let mac_str = bootycall_core::normalize_mac(&mac);
    // SEC-5: reject `%0a`-injected or otherwise malformed MACs before they
    // reach the state store (would flood SEC-4's map) or the returned iPXE
    // script (would inject newlines into the response body).
    if !bootycall_core::is_valid_mac(&mac_str) {
        return (StatusCode::BAD_REQUEST, "Invalid MAC address\n").into_response();
    }
    let client_ip = client_addr.ip().to_string();

    // A missing or invalid boot template is a server misconfiguration, not a
    // client error — log it and return 500 rather than panicking per request.
    let boot_template = match state.jinja_env.get_template("boot") {
        Ok(t) => t,
        Err(e) => {
            error!("boot template unavailable for /poll/{}: {:?}", mac_str, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error\n").into_response();
        }
    };

    let outcome = {
        let config_guard = state.config.read();
        decide_poll_outcome(
            &boot_template,
            &config_guard,
            &state.state_store,
            &mac_str,
            &server_host,
        )
    };

    match outcome {
        PollOutcome::Boot { script, cache_mac } => {
            state.state_store.update_host_status(
                &mac_str,
                HostStatus::Booting,
                None,
                None,
                Some(client_ip.clone()),
                None,
            );
            state.state_store.log_event(
                "INFO",
                Some(&mac_str),
                &format!(
                    "Serving boot execution script for target (mapped cache MAC: {cache_mac})"
                ),
            );
            bootycall_log::event!(
                "http_boot_served",
                mac = %mac_str,
                target_mac = %cache_mac,
                client_ip = %client_ip,
            );
            ([(header::CONTENT_TYPE, "text/plain")], script).into_response()
        }
        PollOutcome::RenderFailed => {
            // The boot template failed to render (a server-side data/template
            // fault). Do NOT transition the host to Booting — mark it Failed,
            // emit an event, and return a non-empty HTTP 500 body rather than
            // the old empty-200 that left the client stuck with no signal.
            state.state_store.update_host_status(
                &mac_str,
                HostStatus::Failed,
                None,
                None,
                Some(client_ip.clone()),
                None,
            );
            state
                .state_store
                .log_event("ERROR", Some(&mac_str), "Boot script render failed");
            bootycall_log::event!(
                "http_boot_render_failed",
                mac = %mac_str,
                client_ip = %client_ip,
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain")],
                "#!ipxe\necho BootyCall: internal error rendering boot script\n",
            )
                .into_response()
        }
        PollOutcome::Poll => {
            state.state_store.update_host_status(
                &mac_str,
                HostStatus::Polling,
                None,
                None,
                Some(client_ip),
                None,
            );
            // Return the poll retry script (endless loop until a target
            // configuration is assigned).
            let retry_script = format!(
                "#!ipxe\nprompt --key 0x02 --timeout 4000 BootyCall: Press Ctrl-B for manual override... \\\n  && chain --autofree http://{}/ipxemenu \\\n  || chain --autofree http://{}/poll/{}\n",
                server_host, server_host, mac_str
            );
            ([(header::CONTENT_TYPE, "text/plain")], retry_script).into_response()
        }
    }
}

// /ipxemenu endpoint fallback for manual choice
async fn menu_handler(State(state): State<ServerState>, headers: HeaderMap) -> impl IntoResponse {
    let (server_host, hosts_list) = {
        let config_guard = state.config.read();
        (
            advertised_host_port(&config_guard.server, &headers),
            config_guard.hosts.clone(),
        )
    };

    let menu_template = match state.jinja_env.get_template("ipxemenu") {
        Ok(t) => t,
        Err(e) => {
            error!("iPXE menu template unavailable: {:?}", e);
            return (
                [(header::CONTENT_TYPE, "text/plain")],
                "#!ipxe\necho Menu error\nexit\n".to_string(),
            );
        }
    };

    let rendered = match menu_template.render(context!(
        server_ip_port => server_host,
        hosts => hosts_list
    )) {
        Ok(res) => res,
        Err(e) => {
            error!("Failed to render minijinja iPXE menu: {:?}", e);
            "#!ipxe\necho Menu error\nexit\n".to_string()
        }
    };

    ([(header::CONTENT_TYPE, "text/plain")], rendered)
}

/// Maximum wallpaper filenames collected from one directory scan. Bounds the
/// per-request allocation so a huge wallpapers directory can't blow up memory
/// (issue 001).
const MAX_WALLPAPER_CANDIDATES: usize = 1024;

/// Collect up to [`MAX_WALLPAPER_CANDIDATES`] image filenames (`.png`/`.jpg`/
/// `.jpeg`) directly under `dir`. Returns an empty vec if `dir` is missing.
async fn collect_wallpaper_candidates(dir: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while candidates.len() < MAX_WALLPAPER_CANDIDATES {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    let lower = file_name.to_lowercase();
                    if lower.ends_with(".png")
                        || lower.ends_with(".jpg")
                        || lower.ends_with(".jpeg")
                    {
                        candidates.push(file_name);
                    }
                }
                _ => break,
            }
        }
    }
    candidates
}

// Dynamic wallpaper selection endpoint
async fn wallpaper_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(query): Query<WallpaperQuery>,
) -> impl IntoResponse {
    let server_host = {
        let config_guard = state.config.read();
        advertised_host_port(&config_guard.server, &headers)
    };

    // Determine target width & height
    let mut target_width = query.width;
    let mut target_height = query.height;

    if query.hd_video.as_deref() == Some("true") {
        target_width = Some(1920);
        target_height = Some(1080);
    }

    // Wallpapers live under the configured static dir (resolved independently of
    // the process CWD), not a hardcoded `./static`.
    let wallpapers_root = {
        let config_guard = state.config.read();
        config_guard.server.static_dir.join("wallpapers")
    };

    // Walk directories to select a wallpaper
    let mut selected_path = None;

    // First, check if a specific resolution directory is requested and exists.
    // Route the resolution segment through `safe_join` for defence in depth —
    // via the blocking pool, since it canonicalises (see `safe_join_blocking`).
    if let (Some(w), Some(h)) = (target_width, target_height) {
        let res_dir_name = format!("{}x{}", w, h);
        if let Some(res_path) =
            safe_join_blocking(wallpapers_root.clone(), res_dir_name.clone()).await
        {
            let candidates = collect_wallpaper_candidates(&res_path).await;
            if !candidates.is_empty() {
                use rand::seq::IndexedRandom;
                let mut rng = rand::rng();
                if let Some(chosen) = candidates.choose(&mut rng) {
                    selected_path = Some(format!("wallpapers/{}/{}", res_dir_name, chosen));
                }
            }
        }
    }

    // Fallback: check main wallpapers directory
    if selected_path.is_none() {
        let candidates = collect_wallpaper_candidates(&wallpapers_root).await;
        if !candidates.is_empty() {
            use rand::seq::IndexedRandom;
            let mut rng = rand::rng();
            if let Some(chosen) = candidates.choose(&mut rng) {
                selected_path = Some(format!("wallpapers/{}", chosen));
            }
        }
    }

    let script = match selected_path {
        Some(path) => {
            // Determine resolution parameters for ipxe console setup
            let w = target_width.unwrap_or(1024);
            let h = target_height.unwrap_or(768);
            format!(
                "#!ipxe\nconsole --x {} --y {} --picture http://{}/static/{} --depth 32 --keep || exit\n",
                w, h, server_host, path
            )
        }
        None => {
            // Safe fallback
            let w = target_width.unwrap_or(1024);
            let h = target_height.unwrap_or(768);
            format!("#!ipxe\nconsole --x {} --y {} || exit\n", w, h)
        }
    };

    ([(header::CONTENT_TYPE, "text/plain")], script)
}

// API endpoint: GET /api/status
async fn api_status_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    // Gated behind api_token when configured (exposes host MACs/IPs).
    check_api_token(&state, &headers)?;

    let hosts = state.state_store.list_hosts();
    let configs = {
        let config_guard = state.config.read();
        config_guard.hosts.clone()
    };

    Ok(Json(StatusResponse { hosts, configs }))
}

// API endpoint: GET /api/logs
async fn api_logs_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    // Gated behind api_token when configured (exposes log history).
    check_api_token(&state, &headers)?;

    let logs = state.state_store.list_logs();
    Ok(Json(logs))
}

// API endpoint: GET /api/health — unauthenticated readiness probe. Healthy when
// every configured host has non-empty cached kernel+initrd artifacts ready to
// serve; degraded (HTTP 503) otherwise. Suitable for a systemd watchdog / LB.
async fn api_health_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let (cache_dir, hosts) = {
        let config_guard = state.config.read();
        (
            config_guard.server.cache_dir.clone(),
            config_guard.hosts.clone(),
        )
    };
    let hosts_total = hosts.len();

    // `host_cache_ready` stats two files per configured host with blocking
    // `std::fs`, so the whole fleet sweep runs on the blocking pool — a load
    // balancer polling /api/health against a large fleet must not stall
    // runtime workers (issue 070; see the convention on `safe_join_blocking`).
    let not_ready: Vec<String> = match tokio::task::spawn_blocking(move || {
        hosts
            .into_iter()
            .filter(|h| !bootycall_extractor::host_cache_ready(&h.mac, &cache_dir))
            .map(|h| h.name)
            .collect()
    })
    .await
    {
        Ok(names) => names,
        Err(e) => {
            // The sweep task panicked or was cancelled — report a server
            // error rather than claiming the box is healthy or degraded.
            error!("health readiness sweep task failed: {e:?}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "hosts_total": hosts_total,
                    "hosts_not_ready": serde_json::Value::Null,
                })),
            );
        }
    };

    let healthy = not_ready.is_empty();
    let status_code = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "status": if healthy { "healthy" } else { "degraded" },
        "hosts_total": hosts_total,
        "hosts_not_ready": not_ready,
    });

    (status_code, Json(body))
}

/// Enforce the optional `api_token`: when one is configured, require a matching
/// `X-API-Token` header (constant-time), else `Err(UNAUTHORIZED)`. When no token
/// is configured the endpoint stays open (backwards-compatible). Shared by the
/// mutating `/api/override` and the read `/api/status` + `/api/logs` endpoints.
fn check_api_token(state: &ServerState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let config_guard = state.config.read();
    if let Some(expected) = config_guard.server.api_token.as_deref() {
        let provided = headers
            .get("X-API-Token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(())
}

/// Length-generic constant-time token comparison so neither the value nor the
/// length of the secret token is observable via response timing.
///
/// Both inputs are first hashed to a fixed 32-byte SHA-256 digest, then the
/// digests are compared byte-for-byte with no early return. Because the compare
/// is always over 32 bytes regardless of input length, a wrong-length token is
/// indistinguishable from a wrong-value one — the previous `a.len() != b.len()`
/// early return leaked the secret's length. Different-length inputs still
/// compare unequal (their digests differ), so correctness is preserved.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let ha = Sha256::digest(a);
    let hb = Sha256::digest(b);
    let mut diff: u8 = 0;
    for (x, y) in ha.iter().zip(hb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// API endpoint: POST /api/override
async fn api_override_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(payload): Json<OverrideRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // SEC-3: when an api_token is configured, mutating endpoints require it via
    // the `X-API-Token` header. Absent config leaves the endpoint
    // unauthenticated (backwards-compatible for hosts already sitting behind a
    // reverse proxy or bound to localhost).
    check_api_token(&state, &headers)?;

    let mac_str = bootycall_core::normalize_mac(&payload.mac);
    // SEC-5: reject malformed MACs early — otherwise the state store logs
    // and stores whatever the client sent, feeding SEC-4.
    if !bootycall_core::is_valid_mac(&mac_str) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Validate target configuration exists
    let target_exists = {
        let config_guard = state.config.read();
        config_guard.hosts.iter().any(|h| h.name == payload.target)
    };

    if !target_exists {
        return Err(StatusCode::BAD_REQUEST);
    }

    state.state_store.update_host_status(
        &mac_str,
        HostStatus::Polling,
        None,
        Some(payload.target.clone()),
        None,
        None,
    );

    state.state_store.log_event(
        "INFO",
        Some(&mac_str),
        &format!("Assigned manual override target: {}", payload.target),
    );

    bootycall_log::event!(
        "http_override_assigned",
        mac = %mac_str,
        target = %payload.target,
    );

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// Runs the Axum HTTP routing server, handling iPXE client scripting and the dashboard.
pub async fn run_http_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
) -> Result<(), std::io::Error> {
    // Initialise minijinja environment. A template that fails to compile (e.g.
    // after a bad edit to MENU_TEMPLATE/BOOT_TEMPLATE) surfaces as a clean
    // startup error instead of a panic — run_http_server returns io::Error.
    let mut env = minijinja::Environment::new();
    env.add_template("ipxemenu", MENU_TEMPLATE).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to register ipxemenu template: {e}"),
        )
    })?;
    env.add_template("boot", BOOT_TEMPLATE).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to register boot template: {e}"),
        )
    })?;
    let jinja_env = Arc::new(env);

    let server_state = ServerState {
        config,
        state_store,
        jinja_env,
    };

    let app = Router::new()
        .route("/", get(root_redirect))
        .route("/ui/{*path}", get(ui_handler))
        .route("/static/{*path}", get(serve_static_file))
        .route("/cache/{*path}", get(serve_cache_file))
        .route("/start", get(start_handler))
        .route("/poll/{mac}", get(poll_handler))
        .route("/ipxemenu", get(menu_handler))
        .route("/dynamic/wallpaper.ipxe", get(wallpaper_handler))
        .route("/api/status", get(api_status_handler))
        .route("/api/logs", get(api_logs_handler))
        .route("/api/health", get(api_health_handler))
        .route("/api/override", post(api_override_handler))
        .with_state(server_state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    info!("HTTP Server listening on http://{}", bind_addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_equal_tokens() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_different_values() {
        assert!(!constant_time_eq(b"secret-token", b"secret-toke0"));
        assert!(!constant_time_eq(b"token", b"other"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        // The hash-then-compare approach must still reject unequal-length
        // tokens (previously the length branch handled this).
        assert!(!constant_time_eq(b"short", b"a-much-longer-token"));
        assert!(!constant_time_eq(b"token", b"tokenX"));
        assert!(!constant_time_eq(b"", b"nonempty"));
    }

    #[test]
    fn content_type_for_covers_key_extensions() {
        assert_eq!(
            content_type_for("boot/x64/ipxe.efi"),
            "application/octet-stream"
        );
        assert_eq!(content_type_for("wallpapers/bg.png"), "image/png");
        assert_eq!(content_type_for("data.json"), "application/json");
        assert_eq!(content_type_for("script.ipxe"), "text/plain");
        assert_eq!(content_type_for("some-file"), "application/octet-stream");
    }

    #[tokio::test]
    async fn serve_file_from_dir_maps_status_codes() {
        // Guards the issue-070 rework (safe_join + stat moved off the runtime
        // threads): the 200/404/403 contract must not change.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("file.bin"), b"DATA").unwrap();

        // Existing file → 200.
        let ok = serve_file_from_dir(dir.path().to_path_buf(), "file.bin".to_string())
            .await
            .expect("existing file must serve");
        assert_eq!(ok.status(), StatusCode::OK);

        // Missing file → 404.
        assert_eq!(
            serve_file_from_dir(dir.path().to_path_buf(), "missing.bin".to_string())
                .await
                .unwrap_err(),
            StatusCode::NOT_FOUND
        );

        // Directory (exists, not a file) → 404.
        assert_eq!(
            serve_file_from_dir(dir.path().to_path_buf(), "subdir".to_string())
                .await
                .unwrap_err(),
            StatusCode::NOT_FOUND
        );

        // Traversal → 403.
        assert_eq!(
            serve_file_from_dir(dir.path().to_path_buf(), "../etc/passwd".to_string())
                .await
                .unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }

    fn test_config(hosts: Vec<HostConfig>) -> Config {
        Config {
            server: ServerConfig {
                http_bind: "0.0.0.0:8080".to_string(),
                tftp_bind: "0.0.0.0:69".to_string(),
                tftp_root: "/tmp".into(),
                proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
                cache_dir: "/tmp/cache".into(),
                static_dir: "./static".into(),
                default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
                default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
                default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
                oled_enabled: false,
                oled_brightness: 255,
                api_token: None,
                max_artifact_bytes: None,
                advertised_host: None,
                allowed_hosts: Vec::new(),
            },
            hosts,
        }
    }

    fn headers_with_host(host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        headers
    }

    #[test]
    fn host_without_port_strips_only_port_suffixes() {
        assert_eq!(host_without_port("127.0.0.1:26080"), "127.0.0.1");
        assert_eq!(
            host_without_port("boot.example.internal"),
            "boot.example.internal"
        );
        assert_eq!(
            host_without_port("boot.example.internal:8080"),
            "boot.example.internal"
        );
        // Bracketed IPv6 with and without a port.
        assert_eq!(host_without_port("[::1]:8080"), "::1");
        assert_eq!(host_without_port("[fd00::2]"), "fd00::2");
        // Unbracketed IPv6 has no unambiguous port separator — untouched.
        assert_eq!(host_without_port("::1"), "::1");
        // Non-numeric or empty "port" is not a port.
        assert_eq!(host_without_port("host:abc"), "host:abc");
        assert_eq!(host_without_port("host:"), "host:");
    }

    #[test]
    fn advertised_host_wins_over_any_host_header() {
        let mut config = test_config(vec![]);
        config.server.advertised_host = Some("boot.internal:8080".to_string());
        config.server.allowed_hosts = vec!["other.example".to_string()];

        let resolved =
            advertised_host_port(&config.server, &headers_with_host("attacker.example:9999"));
        assert_eq!(resolved, "boot.internal:8080");
    }

    #[test]
    fn allowlisted_host_header_is_reflected_with_its_port() {
        let mut config = test_config(vec![]);
        config.server.allowed_hosts = vec!["192.168.1.10".to_string()];

        let resolved =
            advertised_host_port(&config.server, &headers_with_host("192.168.1.10:8080"));
        assert_eq!(resolved, "192.168.1.10:8080");

        // Hostname comparison is case-insensitive (DNS names are).
        config.server.allowed_hosts = vec!["Boot.Internal".to_string()];
        let resolved =
            advertised_host_port(&config.server, &headers_with_host("boot.internal:8080"));
        assert_eq!(resolved, "boot.internal:8080");
    }

    #[test]
    fn spoofed_host_header_is_replaced_by_first_allowed_host() {
        let mut config = test_config(vec![]);
        config.server.allowed_hosts = vec!["192.168.1.10".to_string(), "boot.internal".to_string()];

        let resolved =
            advertised_host_port(&config.server, &headers_with_host("attacker.example:8080"));
        assert_eq!(resolved, "192.168.1.10:8080");
        assert!(!resolved.contains("attacker.example"));

        // A missing Host header takes the same trusted fallback.
        let resolved = advertised_host_port(&config.server, &HeaderMap::new());
        assert_eq!(resolved, "192.168.1.10:8080");
    }

    #[test]
    fn ipv6_allowed_host_fallback_is_bracketed() {
        let mut config = test_config(vec![]);
        config.server.allowed_hosts = vec!["fd00::2".to_string()];

        let resolved = advertised_host_port(&config.server, &headers_with_host("attacker.example"));
        assert_eq!(resolved, "[fd00::2]:8080");
    }

    #[test]
    fn legacy_reflection_only_without_advertised_or_allowlist() {
        let config = test_config(vec![]);
        let resolved =
            advertised_host_port(&config.server, &headers_with_host("anything.example:1234"));
        assert_eq!(resolved, "anything.example:1234");

        let resolved = advertised_host_port(&config.server, &HeaderMap::new());
        assert_eq!(resolved, "localhost:8080");
    }

    fn test_host(mac: &str, name: &str) -> HostConfig {
        HostConfig {
            mac: mac.to_string(),
            name: name.to_string(),
            image_path: "/tmp/img.iso".into(),
            bootloader: None,
            kernel_path: None,
            initrd_path: None,
            cmdline: None,
        }
    }

    #[test]
    fn poll_outcome_boot_for_configured_host() {
        let mut env = minijinja::Environment::new();
        env.add_template("boot", BOOT_TEMPLATE).unwrap();
        let template = env.get_template("boot").unwrap();
        let config = test_config(vec![test_host("aa:bb:cc:dd:ee:ff", "node1")]);
        let store = StateStore::new();

        let outcome = decide_poll_outcome(
            &template,
            &config,
            &store,
            "aa:bb:cc:dd:ee:ff",
            "localhost:8080",
        );
        match outcome {
            PollOutcome::Boot { script, cache_mac } => {
                assert!(script.starts_with("#!ipxe"));
                assert!(script.contains("Booting node1"));
                assert_eq!(cache_mac, "aa:bb:cc:dd:ee:ff");
            }
            _ => panic!("expected Boot for a directly-configured host"),
        }
    }

    #[test]
    fn poll_outcome_render_failure_surfaces() {
        // A boot template that references a variable never provided by the
        // render context errors under strict-undefined behaviour — exactly the
        // render-failure branch poll_handler must handle (issue 010).
        let mut env = minijinja::Environment::new();
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        env.add_template("boot", "#!ipxe\n{{ never_provided_variable }}\n")
            .unwrap();
        let template = env.get_template("boot").unwrap();
        let config = test_config(vec![test_host("aa:bb:cc:dd:ee:ff", "node1")]);
        let store = StateStore::new();

        let outcome = decide_poll_outcome(
            &template,
            &config,
            &store,
            "aa:bb:cc:dd:ee:ff",
            "localhost:8080",
        );
        assert!(
            matches!(outcome, PollOutcome::RenderFailed),
            "a template that errors at render time must map to RenderFailed"
        );
    }

    #[test]
    fn poll_outcome_poll_when_override_target_missing() {
        // Host is not configured directly but is registered with an override
        // target that no longer exists in config — keep polling, do not boot.
        let mut env = minijinja::Environment::new();
        env.add_template("boot", BOOT_TEMPLATE).unwrap();
        let template = env.get_template("boot").unwrap();
        let config = test_config(vec![]);
        let store = StateStore::new();
        store.update_host_status(
            "11:22:33:44:55:66",
            HostStatus::Polling,
            None,
            Some("ghost-target".to_string()),
            None,
            None,
        );

        let outcome = decide_poll_outcome(
            &template,
            &config,
            &store,
            "11:22:33:44:55:66",
            "localhost:8080",
        );
        assert!(
            matches!(outcome, PollOutcome::Poll),
            "an override pointing at a missing target must keep polling"
        );
    }

    #[test]
    fn poll_outcome_poll_for_unknown_host() {
        let mut env = minijinja::Environment::new();
        env.add_template("boot", BOOT_TEMPLATE).unwrap();
        let template = env.get_template("boot").unwrap();
        let config = test_config(vec![]);
        let store = StateStore::new();

        let outcome = decide_poll_outcome(
            &template,
            &config,
            &store,
            "99:99:99:99:99:99",
            "localhost:8080",
        );
        assert!(matches!(outcome, PollOutcome::Poll));
    }
}
