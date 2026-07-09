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
use std::path::Path;
use std::sync::Arc;

use bootycall_core::config::{Config, HostConfig};
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

async fn serve_file_from_dir(dir: &Path, relative_path: &str) -> Result<Response, StatusCode> {
    let full_path = match bootycall_core::safe_join(dir, relative_path) {
        Some(p) => p,
        None => return Err(StatusCode::FORBIDDEN),
    };

    if !full_path.exists() || !full_path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }

    let file = match tokio::fs::File::open(&full_path).await {
        Ok(f) => f,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    let content_type = content_type_for(relative_path);

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
    serve_file_from_dir(&static_dir, &path).await
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
    serve_file_from_dir(&cache_dir, &path).await
}

/// Extract the client-facing `Host:` header (used to build the URLs the
/// generated iPXE scripts hand back), falling back to the dev default. Kept in
/// one place so the `localhost:8080` default lives at a single site.
fn host_header(headers: &HeaderMap) -> &str {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080")
}

// /start endpoint - chains client to MAC specific poll endpoint
async fn start_handler(headers: HeaderMap) -> impl IntoResponse {
    let host_hdr = host_header(&headers);

    let script = format!(
        "#!ipxe\necho BootyCall starting...\nchain --autofree --replace http://{}/poll/${{mac:hexhyp}}\n",
        host_hdr
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
fn decide_poll_outcome(
    boot_template: &minijinja::Template<'_, '_>,
    config: &Config,
    state_store: &StateStore,
    mac_str: &str,
    host_hdr: &str,
) -> PollOutcome {
    // 1. Host configured directly in yaml.
    if let Some(host_config) = config.find_host(mac_str) {
        return match boot_template.render(context! {
            name => host_config.name.as_str(),
            server_ip_port => host_hdr,
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
            server_ip_port => host_hdr,
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
    let host_hdr = host_header(&headers);

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
            host_hdr,
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
                host_hdr, host_hdr, mac_str
            );
            ([(header::CONTENT_TYPE, "text/plain")], retry_script).into_response()
        }
    }
}

// /ipxemenu endpoint fallback for manual choice
async fn menu_handler(State(state): State<ServerState>, headers: HeaderMap) -> impl IntoResponse {
    let host_hdr = host_header(&headers);

    let hosts_list = {
        let config_guard = state.config.read();
        config_guard.hosts.clone()
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
        server_ip_port => host_hdr,
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
    let host_hdr = host_header(&headers);

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
    // Route the resolution segment through `safe_join` for defence in depth.
    if let (Some(w), Some(h)) = (target_width, target_height) {
        let res_dir_name = format!("{}x{}", w, h);
        if let Some(res_path) = bootycall_core::safe_join(&wallpapers_root, &res_dir_name) {
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
                w, h, host_hdr, path
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

    let not_ready: Vec<String> = hosts
        .iter()
        .filter(|h| !bootycall_extractor::host_cache_ready(&h.mac, &cache_dir))
        .map(|h| h.name.clone())
        .collect();

    let healthy = not_ready.is_empty();
    let status_code = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "status": if healthy { "healthy" } else { "degraded" },
        "hosts_total": hosts.len(),
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

    fn test_config(hosts: Vec<HostConfig>) -> Config {
        use bootycall_core::config::ServerConfig;
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
                api_token: None,
                max_artifact_bytes: None,
            },
            hosts,
        }
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
