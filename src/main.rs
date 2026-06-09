mod mailer;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Config {
    listen: String,
    token: String,
    offline_timeout: u64,
    email: mailer::EmailConfig,
}

impl Config {
    fn from_env() -> Config {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let to = env("NETDOG_EMAIL_TO", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        Config {
            listen: env("NETDOG_LISTEN", "0.0.0.0:8688"),
            token: env("NETDOG_TOKEN", ""),
            offline_timeout: env("NETDOG_OFFLINE_TIMEOUT", "90").parse().unwrap_or(90),
            email: mailer::EmailConfig {
                enabled: env("NETDOG_EMAIL_ENABLED", "true") == "true",
                to,
                username: env("SMTP_USERNAME", ""),
                password: env("SMTP_PASSWORD", ""),
                host: env("SMTP_HOST", ""),
                smtp_port: env("SMTP_PORT", "465").parse().unwrap_or(465),
                from_address: env("SMTP_FROM", ""),
                display_name: env("SMTP_DISPLAY_NAME", "netdog-server"),
                use_ssl: env("SMTP_USE_SSL", "true") == "true",
            },
        }
    }
}

/// Heartbeat payload sent by netdog agents.
#[derive(Deserialize, Default)]
struct Heartbeat {
    device: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    healthy: bool,
    #[serde(default)]
    consecutive_fails: u32,
    #[serde(default)]
    total_restarts: u64,
    #[serde(default)]
    note: String,
}

#[derive(Clone, Serialize)]
struct DeviceState {
    device: String,
    service: String,
    online: bool,
    healthy: bool,
    consecutive_fails: u32,
    total_restarts: u64,
    last_seen: i64,
    note: String,
}

struct App {
    cfg: Config,
    devices: Mutex<HashMap<String, DeviceState>>,
}
type Shared = Arc<App>;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Send an email in a blocking task so it never stalls the async runtime.
fn notify(app: &Shared, subject: String, body: String) {
    if !app.cfg.email.enabled {
        println!("[{}] (email disabled) {subject}", stamp());
        return;
    }
    let email = app.cfg.email.clone();
    tokio::task::spawn_blocking(move || match mailer::send(&email, &subject, &body) {
        Ok(()) => println!("[{}] alert email sent: {subject}", stamp()),
        Err(e) => eprintln!("[{}] alert email FAILED: {e} ({subject})", stamp()),
    });
}

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();
    if cfg.token.is_empty() {
        eprintln!("WARNING: NETDOG_TOKEN is empty — anyone can connect. Set a token!");
    }
    let app: Shared = Arc::new(App {
        cfg: cfg.clone(),
        devices: Mutex::new(HashMap::new()),
    });

    // Dead-man sweeper: mark devices offline when heartbeats stop arriving.
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                sweep(&app).await;
            }
        });
    }

    let router = Router::new()
        .route("/", get(status_page))
        .route("/api/status", get(status_json))
        .route("/agent", get(ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .unwrap_or_else(|e| panic!("bind {}: {e}", cfg.listen));
    println!("[{}] netdog-server listening on {}", stamp(), cfg.listen);
    axum::serve(listener, router).await.unwrap();
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    State(app): State<Shared>,
) -> axum::response::Response {
    if q.get("token").map(String::as_str) != Some(app.cfg.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let device = q.get("device").cloned().unwrap_or_else(|| "unknown".to_string());
    ws.on_upgrade(move |socket| handle_socket(socket, device, app))
}

async fn handle_socket(mut socket: WebSocket, device: String, app: Shared) {
    println!("[{}] agent connected: {device}", stamp());
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(t) => {
                if let Ok(mut hb) = serde_json::from_str::<Heartbeat>(&t) {
                    if hb.device.is_empty() {
                        hb.device = device.clone();
                    }
                    on_heartbeat(&app, hb).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    println!("[{}] agent disconnected: {device}", stamp());
    // Offline alerting is handled by the sweeper (no heartbeat -> timeout).
}

async fn on_heartbeat(app: &Shared, hb: Heartbeat) {
    let mut alerts: Vec<(String, String)> = Vec::new();
    {
        let mut devices = app.devices.lock().await;
        let prev = devices.get(&hb.device).cloned();
        let was_online = prev.as_ref().map(|d| d.online).unwrap_or(false);
        let was_healthy = prev.as_ref().map(|d| d.healthy).unwrap_or(true);

        let state = DeviceState {
            device: hb.device.clone(),
            service: hb.service.clone(),
            online: true,
            healthy: hb.healthy,
            consecutive_fails: hb.consecutive_fails,
            total_restarts: hb.total_restarts,
            last_seen: now(),
            note: hb.note.clone(),
        };
        devices.insert(hb.device.clone(), state);

        if !was_online {
            alerts.push((
                format!("[netdog] 🟢 {} 已上线", hb.device),
                format!("设备: {}\n服务: {}\n状态: 已连接到告警服务器\n时间: {}\n", hb.device, hb.service, stamp()),
            ));
        }
        if was_healthy && !hb.healthy {
            alerts.push((
                format!("[netdog] ⚠️ {} 代理异常", hb.device),
                format!("设备: {}\n服务: {}\n状态: 代理连通性异常 (设备在线)\n连续失败: {}\n备注: {}\n时间: {}\n",
                    hb.device, hb.service, hb.consecutive_fails, hb.note, stamp()),
            ));
        } else if !was_healthy && hb.healthy {
            alerts.push((
                format!("[netdog] ✅ {} 代理已恢复", hb.device),
                format!("设备: {}\n服务: {}\n状态: 代理连通性已恢复\n时间: {}\n", hb.device, hb.service, stamp()),
            ));
        }
    }
    for (s, b) in alerts {
        notify(app, s, b);
    }
}

async fn sweep(app: &Shared) {
    let timeout = app.cfg.offline_timeout as i64;
    let mut alerts: Vec<(String, String)> = Vec::new();
    {
        let mut devices = app.devices.lock().await;
        let t = now();
        for d in devices.values_mut() {
            if d.online && t - d.last_seen > timeout {
                d.online = false;
                alerts.push((
                    format!("[netdog] 🔴 {} 掉线", d.device),
                    format!(
                        "设备: {}\n服务: {}\n状态: 失联 (超过 {}s 未收到心跳)\n最后心跳: {}s 前\n时间: {}\n\n该设备可能整机离线/断电/断网，无法自行发送告警，由服务端代为通知。",
                        d.device, d.service, timeout, t - d.last_seen, stamp()
                    ),
                ));
            }
        }
    }
    for (s, b) in alerts {
        notify(app, s, b);
    }
}

async fn status_json(State(app): State<Shared>) -> Json<Vec<DeviceState>> {
    let devices = app.devices.lock().await;
    let mut v: Vec<DeviceState> = devices.values().cloned().collect();
    v.sort_by(|a, b| a.device.cmp(&b.device));
    Json(v)
}

async fn status_page(State(app): State<Shared>) -> Html<String> {
    let devices = app.devices.lock().await;
    let mut rows = String::new();
    let t = now();
    let mut list: Vec<&DeviceState> = devices.values().collect();
    list.sort_by(|a, b| a.device.cmp(&b.device));
    for d in list {
        let (color, label) = if !d.online {
            ("#dc2626", "OFFLINE")
        } else if !d.healthy {
            ("#d97706", "UNHEALTHY")
        } else {
            ("#16a34a", "OK")
        };
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td><b style=\"color:{}\">{}</b></td><td>{}</td><td>{}</td><td>{}s ago</td><td>{}</td></tr>",
            html_escape(&d.device), html_escape(&d.service), color, label,
            d.consecutive_fails, d.total_restarts,
            t - d.last_seen, html_escape(&d.note)
        ));
    }
    if rows.is_empty() {
        rows = "<tr><td colspan=7>No agents have connected yet.</td></tr>".to_string();
    }
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>netdog-server</title>\
         <meta http-equiv=refresh content=5>\
         <style>body{{font-family:system-ui,sans-serif;margin:2rem;color:#222}}\
         table{{border-collapse:collapse;width:100%}}\
         th,td{{border:1px solid #ddd;padding:6px 10px;text-align:left}}\
         th{{background:#f3f4f6}}</style></head><body>\
         <h2>netdog-server</h2><p>Alert server · {} devices · offline timeout {}s · {}</p>\
         <table><tr><th>Device</th><th>Service</th><th>Status</th><th>Fails</th>\
         <th>Restarts</th><th>Last seen</th><th>Note</th></tr>{}</table></body></html>",
        devices.len(), app.cfg.offline_timeout, stamp(), rows
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
