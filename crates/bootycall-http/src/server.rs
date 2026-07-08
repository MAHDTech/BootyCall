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

    let ext = Path::new(relative_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let content_type = match ext.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "html" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        _ => "application/octet-stream",
    };

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    Ok(([(header::CONTENT_TYPE, content_type)], body).into_response())
}

async fn serve_asset(path: &str) -> Response {
    match Asset::get(path) {
        Some(content) => {
            let mime_type = match path.split('.').next_back() {
                Some("html") => "text/html",
                Some("css") => "text/css",
                Some("js") => "application/javascript",
                _ => "application/octet-stream",
            };
            (
                [(header::CONTENT_TYPE, mime_type)],
                content.data.into_owned(),
            )
                .into_response()
        }
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

// Serving static wallpaper files from the disk static dir
async fn serve_static_file(AxumPath(path): AxumPath<String>) -> Result<Response, StatusCode> {
    serve_file_from_dir(Path::new("./static"), &path).await
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

// /start endpoint - chains client to MAC specific poll endpoint
async fn start_handler(headers: HeaderMap) -> impl IntoResponse {
    let host_hdr = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");

    let script = format!(
        "#!ipxe\necho BootyCall starting...\nchain --autofree --replace http://{}/poll/${{mac:hexhyp}}\n",
        host_hdr
    );

    ([(header::CONTENT_TYPE, "text/plain")], script)
}

// /poll/{mac} endpoint
async fn poll_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    AxumPath(mac): AxumPath<String>,
) -> impl IntoResponse {
    let host_hdr = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");

    let mac_str = bootycall_core::normalize_mac(&mac);
    let client_ip = client_addr.ip().to_string();

    let (boot_script, should_update_booting, target_mac_to_use) = {
        let config_guard = state.config.read();

        // 1. Check if host is configured directly in yaml
        if let Some(host_config) = config_guard.find_host(&mac_str) {
            let cmdline = host_config.cmdline.as_deref().unwrap_or("");
            let script = format!(
                "#!ipxe\necho Booting {}...\nkernel http://{}/cache/{}/kernel {}\ninitrd http://{}/cache/{}/initrd\nboot\n",
                host_config.name, host_hdr, host_config.mac, cmdline, host_hdr, host_config.mac
            );
            (Some(script), true, Some(host_config.mac.clone()))
        } else {
            // 2. Check if host is registered and has a manual override target
            let host_state = state.state_store.get_host(&mac_str);
            if let Some(hs) = host_state {
                if let Some(ref target_name) = hs.assigned_target {
                    if let Some(target_config) =
                        config_guard.hosts.iter().find(|h| &h.name == target_name)
                    {
                        let cmdline = target_config.cmdline.as_deref().unwrap_or("");
                        let script = format!(
                            "#!ipxe\necho Booting target {} for host {}...\nkernel http://{}/cache/{}/kernel {}\ninitrd http://{}/cache/{}/initrd\nboot\n",
                            target_config.name,
                            mac_str,
                            host_hdr,
                            target_config.mac,
                            cmdline,
                            host_hdr,
                            target_config.mac
                        );
                        (Some(script), true, Some(target_config.mac.clone()))
                    } else {
                        (None, false, None)
                    }
                } else {
                    (None, false, None)
                }
            } else {
                (None, false, None)
            }
        }
    };

    if let Some(script) = boot_script {
        if should_update_booting {
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
                    "Serving boot execution script for target (mapped cache MAC: {:?})",
                    target_mac_to_use
                ),
            );
        }
        return ([(header::CONTENT_TYPE, "text/plain")], script).into_response();
    }

    // Update state store to Polling if not booted
    state.state_store.update_host_status(
        &mac_str,
        HostStatus::Polling,
        None,
        None,
        Some(client_ip),
        None,
    );

    // Return the poll retry script (endless loop until target configuration is assigned)
    let retry_script = format!(
        "#!ipxe\nprompt --key 0x02 --timeout 4000 BootyCall: Press Ctrl-B for manual override... \\\n  && chain --autofree http://{}/ipxemenu \\\n  || chain --autofree http://{}/poll/{}\n",
        host_hdr, host_hdr, mac_str
    );

    ([(header::CONTENT_TYPE, "text/plain")], retry_script).into_response()
}

// /ipxemenu endpoint fallback for manual choice
async fn menu_handler(State(state): State<ServerState>, headers: HeaderMap) -> impl IntoResponse {
    let host_hdr = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");

    let hosts_list = {
        let config_guard = state.config.read();
        config_guard.hosts.clone()
    };

    let rendered = match state
        .jinja_env
        .get_template("ipxemenu")
        .unwrap()
        .render(context!(
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

// Dynamic wallpaper selection endpoint
async fn wallpaper_handler(
    State(_state): State<ServerState>,
    headers: HeaderMap,
    Query(query): Query<WallpaperQuery>,
) -> impl IntoResponse {
    let host_hdr = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");

    // Determine target width & height
    let mut target_width = query.width;
    let mut target_height = query.height;

    if query.hd_video.as_deref() == Some("true") {
        target_width = Some(1920);
        target_height = Some(1080);
    }

    // Walk directories to select a wallpaper
    let mut selected_path = None;

    // First, check if a specific resolution directory is requested and exists
    if let (Some(w), Some(h)) = (target_width, target_height) {
        let res_dir_name = format!("{}x{}", w, h);
        let res_path_local = Path::new("./static/wallpapers").join(&res_dir_name);

        let mut candidates = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&res_path_local).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file_name = entry.file_name().to_string_lossy().to_string();
                let lower = file_name.to_lowercase();
                if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                    candidates.push(file_name);
                }
            }
        }

        if !candidates.is_empty() {
            use rand::seq::IndexedRandom;
            let mut rng = rand::rng();
            if let Some(chosen) = candidates.choose(&mut rng) {
                selected_path = Some(format!("wallpapers/{}/{}", res_dir_name, chosen));
            }
        }
    }

    // Fallback: check main wallpapers directory
    if selected_path.is_none() {
        let mut candidates = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir("./static/wallpapers").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file_name = entry.file_name().to_string_lossy().to_string();
                let lower = file_name.to_lowercase();
                if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                    candidates.push(file_name);
                }
            }
        }

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
async fn api_status_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let hosts = state.state_store.list_hosts();
    let configs = {
        let config_guard = state.config.read();
        config_guard.hosts.clone()
    };

    Json(StatusResponse { hosts, configs })
}

// API endpoint: GET /api/logs
async fn api_logs_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let logs = state.state_store.list_logs();
    Json(logs)
}

// API endpoint: POST /api/override
async fn api_override_handler(
    State(state): State<ServerState>,
    Json(payload): Json<OverrideRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let mac_str = bootycall_core::normalize_mac(&payload.mac);

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

    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// Runs the Axum HTTP routing server, handling iPXE client scripting and the dashboard.
pub async fn run_http_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
) -> Result<(), std::io::Error> {
    // Initialise minijinja environment
    let mut env = minijinja::Environment::new();
    env.add_template("ipxemenu", MENU_TEMPLATE).unwrap();
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
