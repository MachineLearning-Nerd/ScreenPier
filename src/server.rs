use crate::{
    encoder::EncoderKind,
    media::{HostCommand, OfferResult, QualityMode, run_host},
};
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Clone, Parser)]
#[command(
    version,
    about = "Control this machine's live Wayland desktop through a private browser session"
)]
pub struct Config {
    #[arg(long, required_unless_present = "local_test")]
    pub owner_login: Option<String>,
    #[arg(long, required_unless_present = "local_test")]
    pub public_origin: Option<String>,
    #[arg(long, default_value_t = 6080)]
    pub http_port: u16,
    #[arg(long)]
    pub media_ip: IpAddr,
    #[arg(long, default_value_t = 6081)]
    pub media_port: u16,
    #[arg(long, default_value = "eDP-1")]
    pub output: String,
    #[arg(long, value_enum, default_value = "vaapi")]
    pub encoder: EncoderKind,
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u32).range(200..=10000))]
    pub bitrate_kbps: u32,
    #[arg(
        long,
        help = "Use a loopback-only test endpoint authenticated by TWA_LOCAL_TOKEN (at least 32 characters)"
    )]
    pub local_test: bool,
    #[arg(
        long,
        help = "Write bounded numeric diagnostics to an owner-only JSONL file (8 MiB rollover)"
    )]
    pub diagnostics_log: Option<PathBuf>,
}

#[derive(Clone)]
pub struct Admission {
    origin: String,
    authority: String,
    owner: Option<String>,
    local_token: Option<String>,
}

impl Admission {
    fn from_config(config: &Config) -> Result<Self> {
        if config.http_port == 0 {
            bail!("HTTP port must be nonzero");
        }
        let (origin, owner, local_token) = if config.local_test {
            if !config.media_ip.is_loopback() {
                bail!("local test media must bind to loopback");
            }
            let token = std::env::var("TWA_LOCAL_TOKEN")
                .map_err(|_| anyhow::anyhow!("TWA_LOCAL_TOKEN is required for local test mode"))?;
            if token.len() < 32 || token.len() > 256 || !token.is_ascii() {
                bail!("TWA_LOCAL_TOKEN must contain 32 to 256 ASCII characters");
            }
            (
                format!("http://127.0.0.1:{}", config.http_port),
                None,
                Some(token),
            )
        } else {
            let owner = config
                .owner_login
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("owner login required"))?;
            if !is_tailnet_ip(config.media_ip) {
                bail!("production media must bind to a Tailscale address");
            }
            (
                config
                    .public_origin
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("public HTTPS origin required"))?,
                Some(owner),
                None,
            )
        };
        let uri: Uri = origin.parse()?;
        if uri.scheme_str() != Some(if config.local_test { "http" } else { "https" })
            || uri.path() != "/"
            || uri.query().is_some()
            || uri.authority().is_none()
            || origin.ends_with('/')
        {
            bail!("origin must contain only scheme and authority without a trailing slash");
        }
        Ok(Self {
            authority: uri.authority().unwrap().to_string(),
            origin,
            owner,
            local_token,
        })
    }

    fn check(&self, headers: &HeaderMap, mutation: bool) -> bool {
        if value(headers, "host") != Some(self.authority.as_str()) {
            return false;
        }
        let identity = if let Some(token) = &self.local_token {
            value(headers, "x-twa-local-token")
                .is_some_and(|candidate| constant_eq(token.as_bytes(), candidate.as_bytes()))
        } else {
            value(headers, "tailscale-user-login") == self.owner.as_deref()
        };
        identity
            && (!mutation
                || (value(headers, "origin") == Some(self.origin.as_str())
                    && value(headers, "x-twa-request") == Some("1")))
    }
}

fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => (u32::from(ip) & 0xffc00000) == u32::from(Ipv4Addr::new(100, 64, 0, 0)),
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}
fn value<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok()
}

#[derive(Default, Clone, Serialize)]
pub struct HostStatus {
    pub active: bool,
    pub connected: bool,
    pub input_ready: bool,
    pub lock: String,
    pub state: String,
    pub frames: u64,
    pub bitrate_kbps: u32,
    pub presented_age_ms: Option<u64>,
    pub diagnostics_dropped: u64,
    pub diagnostics_failed: bool,
    pub cleanup: crate::desktop_actor::CleanupHealth,
    pub desktop_shutdown: Option<crate::desktop_actor::ShutdownOutcome>,
}

#[derive(Clone)]
struct AppState {
    admission: Admission,
    commands: SyncSender<HostCommand>,
    status: Arc<Mutex<HostStatus>>,
}

async fn admit(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mutation = request.method() != axum::http::Method::GET;
    if !state.admission.check(request.headers(), mutation) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"Owner authentication or origin rejected"})),
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("content-security-policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; media-src 'self' blob:; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".parse().unwrap());
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfferBody {
    sdp: String,
    #[serde(default)]
    takeover: bool,
    #[serde(default)]
    quality: QualityMode,
}

async fn offer(State(state): State<AppState>, Json(body): Json<OfferBody>) -> Response {
    if body.sdp.len() > 64 * 1024 || body.sdp.is_empty() {
        return failure(StatusCode::BAD_REQUEST, "Invalid offer size");
    }
    let (reply, answer) = oneshot::channel();
    if state
        .commands
        .try_send(HostCommand::Offer {
            sdp: body.sdp,
            takeover: body.takeover,
            quality: body.quality,
            reply,
        })
        .is_err()
    {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "Host is busy");
    }
    match tokio::time::timeout(Duration::from_secs(3), answer).await {
        Ok(Ok(OfferResult::Accepted { sdp, lease, epoch })) => {
            Json(serde_json::json!({"sdp":sdp,"lease":lease,"epoch":epoch})).into_response()
        }
        Ok(Ok(OfferResult::Busy)) => failure(
            StatusCode::CONFLICT,
            "Another tab owns control. Use Take control to replace it.",
        ),
        Ok(Ok(OfferResult::Rejected)) => failure(
            StatusCode::BAD_REQUEST,
            "Offer does not support this desktop stream",
        ),
        _ => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Host did not accept the offer",
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopBody {
    lease: String,
}
async fn stop(State(state): State<AppState>, Json(body): Json<StopBody>) -> Response {
    if body.lease.len() != 64 {
        return failure(StatusCode::BAD_REQUEST, "Invalid lease");
    }
    let (reply, result) = oneshot::channel();
    if state
        .commands
        .try_send(HostCommand::Stop {
            lease: body.lease,
            reply,
        })
        .is_err()
    {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "Host is busy");
    }
    match tokio::time::timeout(Duration::from_secs(3), result).await {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => failure(StatusCode::CONFLICT, "Lease is no longer current"),
        _ => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Stop could not be confirmed",
        ),
    }
}
fn failure(status: StatusCode, error: &str) -> Response {
    (status, Json(serde_json::json!({"error":error}))).into_response()
}

pub async fn run(config: Config) -> Result<()> {
    let http_port = config.http_port;
    let admission = Admission::from_config(&config)?;
    let origin = admission.origin.clone();
    let (commands, receiver) = std::sync::mpsc::sync_channel(8);
    let status = Arc::new(Mutex::new(HostStatus::default()));
    let stopping = Arc::new(AtomicBool::new(false));
    let socket = std::net::UdpSocket::bind((config.media_ip, config.media_port))?;
    socket.set_nonblocking(true)?;
    let state = AppState {
        admission,
        commands,
        status: status.clone(),
    };
    let host_stop = stopping.clone();
    let worker = std::thread::Builder::new()
        .name("desktop-session".into())
        .spawn(move || run_host(config, socket, receiver, status, host_stop))?;
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/api/status",
            get(|State(state): State<AppState>| async move {
                Json(state.status.lock().unwrap().clone())
            }),
        )
        .route("/api/offer", post(offer))
        .route("/api/stop", post(stop))
        .layer(DefaultBodyLimit::max(72 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), admit))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, http_port)).await;
    let listener = match listener {
        Ok(listener) => listener,
        Err(error) => {
            stopping.store(true, Ordering::Release);
            let _ = worker.join();
            return Err(error.into());
        }
    };
    println!("ScreenPier ready. Open {origin}. Stop with Ctrl+C.");
    let shutdown_stop = stopping.clone();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_stop.store(true, Ordering::Release);
        })
        .await;
    stopping.store(true, Ordering::Release);
    let _ = worker.join();
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_quality_defaults_to_clear_and_rejects_unknown_values() {
        let default: OfferBody = serde_json::from_str(r#"{"sdp":"offer"}"#).unwrap();
        assert_eq!(default.quality, QualityMode::Clear);
        let smooth: OfferBody =
            serde_json::from_str(r#"{"sdp":"offer","quality":"smooth"}"#).unwrap();
        assert_eq!(smooth.quality, QualityMode::Smooth);
        let focus: OfferBody =
            serde_json::from_str(r#"{"sdp":"offer","quality":"focus"}"#).unwrap();
        assert_eq!(focus.quality, QualityMode::Focus);
        assert!(
            serde_json::from_str::<OfferBody>(r#"{"sdp":"offer","quality":"unlimited"}"#).is_err()
        );
    }
    #[test]
    fn every_remote_route_requires_the_configured_owner_and_mutations_require_origin_and_csrf() {
        let a = Admission {
            origin: "https://host.ts.net:8443".into(),
            authority: "host.ts.net:8443".into(),
            owner: Some("owner@example.com".into()),
            local_token: None,
        };
        let mut h = HeaderMap::new();
        h.insert("host", "host.ts.net:8443".parse().unwrap());
        assert!(!a.check(&h, false));
        h.insert("tailscale-user-login", "other@example.com".parse().unwrap());
        assert!(!a.check(&h, false));
        h.insert("tailscale-user-login", "owner@example.com".parse().unwrap());
        assert!(a.check(&h, false));
        assert!(!a.check(&h, true));
        h.insert("origin", "https://host.ts.net:8443".parse().unwrap());
        h.insert("x-twa-request", "1".parse().unwrap());
        assert!(a.check(&h, true));
        h.insert("origin", "https://evil.example".parse().unwrap());
        assert!(!a.check(&h, true));
        h.insert("host", "127.0.0.1:6080".parse().unwrap());
        assert!(!a.check(&h, false));
    }
    #[test]
    fn tailscale_only_binding_rejects_public_and_wildcard_addresses() {
        assert!(is_tailnet_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("0.0.0.0".parse().unwrap()));
        assert!(!is_tailnet_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.63.255.255".parse().unwrap()));
    }
}
