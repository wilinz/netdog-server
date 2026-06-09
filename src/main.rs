mod mailer;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Form, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Redirect, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const SESSION_TTL: i64 = 86400; // 1 day
const COOKIE_NAME: &str = "netdog_session";

#[derive(Clone)]
struct Config {
    listen: String,
    token: String,
    offline_timeout: u64,
    panel_user: String,
    panel_password: String,
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
        let mut panel_password = env("PANEL_PASSWORD", "");
        if panel_password.is_empty() {
            panel_password = random_token()[..16].to_string();
            println!(
                "[{}] PANEL_PASSWORD not set — generated one for this run: {}",
                stamp(),
                panel_password
            );
        }
        Config {
            listen: env("NETDOG_LISTEN", "0.0.0.0:8688"),
            token: env("NETDOG_TOKEN", ""),
            offline_timeout: env("NETDOG_OFFLINE_TIMEOUT", "90").parse().unwrap_or(90),
            panel_user: env("PANEL_USER", "admin"),
            panel_password,
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
    sessions: Mutex<HashMap<String, i64>>, // session id -> expiry (unix secs)
}
type Shared = Arc<App>;

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    use std::io::Read;
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((n >> ((i % 16) * 8)) as u8) ^ (i as u8).wrapping_mul(167);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

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
        eprintln!("WARNING: NETDOG_TOKEN is empty — anyone can connect an agent. Set a token!");
    }
    let app: Shared = Arc::new(App {
        cfg: cfg.clone(),
        devices: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
    });

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
        .route("/", get(panel))
        .route("/login", get(login_page).post(do_login))
        .route("/logout", get(logout))
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

// ---------------- auth ----------------

async fn authed(app: &Shared, headers: &HeaderMap) -> bool {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sid = cookie
        .split(';')
        .filter_map(|c| c.trim().strip_prefix(&format!("{COOKIE_NAME}=")))
        .next();
    if let Some(sid) = sid {
        let mut s = app.sessions.lock().await;
        if let Some(&exp) = s.get(sid) {
            if exp > now() {
                return true;
            }
            s.remove(sid);
        }
    }
    false
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn do_login(State(app): State<Shared>, Form(f): Form<LoginForm>) -> Response {
    if ct_eq(&f.username, &app.cfg.panel_user) && ct_eq(&f.password, &app.cfg.panel_password) {
        let sid = random_token();
        app.sessions.lock().await.insert(sid.clone(), now() + SESSION_TTL);
        let mut resp = Redirect::to("/").into_response();
        let cookie = format!(
            "{COOKIE_NAME}={sid}; HttpOnly; Path=/; Max-Age={SESSION_TTL}; SameSite=Lax"
        );
        resp.headers_mut()
            .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
        resp
    } else {
        login_html(true).into_response()
    }
}

async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(sid) = cookie
        .split(';')
        .filter_map(|c| c.trim().strip_prefix(&format!("{COOKIE_NAME}=")))
        .next()
    {
        app.sessions.lock().await.remove(sid);
    }
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!("{COOKIE_NAME}=; HttpOnly; Path=/; Max-Age=0")).unwrap(),
    );
    resp
}

async fn login_page() -> Html<String> {
    login_html(false)
}

fn login_html(error: bool) -> Html<String> {
    let err = if error {
        "<p class=err>Invalid username or password.</p>"
    } else {
        ""
    };
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>netdog · login</title>\
        <meta name=viewport content=\"width=device-width,initial-scale=1\">{STYLE}</head><body>\
        <div class=login><h1>🐕 netdog</h1>{err}\
        <form method=post action=/login>\
        <input name=username placeholder=Username autofocus autocomplete=username>\
        <input name=password type=password placeholder=Password autocomplete=current-password>\
        <button type=submit>Sign in</button></form></div></body></html>"
    ))
}

// ---------------- panel ----------------

async fn panel(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authed(&app, &headers).await {
        return Redirect::to("/login").into_response();
    }
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>netdog panel</title>\
        <meta name=viewport content=\"width=device-width,initial-scale=1\">{STYLE}</head><body>\
        <header><h1>🐕 netdog <span class=sub>alert server</span></h1>\
        <a class=logout href=/logout>Sign out</a></header>\
        <div id=summary class=cards></div>\
        <table><thead><tr><th>Device</th><th>Service</th><th>Status</th>\
        <th>Fails</th><th>Restarts</th><th>Last seen</th><th>Note</th></tr></thead>\
        <tbody id=rows></tbody></table>\
        <p class=foot>offline timeout {}s · auto-refresh 5s · <span id=ts></span></p>\
        {SCRIPT}</body></html>",
        app.cfg.offline_timeout
    ))
    .into_response()
}

async fn status_json(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !authed(&app, &headers).await {
        return (StatusCode::UNAUTHORIZED, "login required").into_response();
    }
    let devices = app.devices.lock().await;
    let mut v: Vec<DeviceState> = devices.values().cloned().collect();
    v.sort_by(|a, b| a.device.cmp(&b.device));
    Json(v).into_response()
}

// ---------------- agent websocket ----------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    State(app): State<Shared>,
) -> Response {
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
}

async fn on_heartbeat(app: &Shared, hb: Heartbeat) {
    let mut alerts: Vec<(String, String)> = Vec::new();
    {
        let mut devices = app.devices.lock().await;
        let prev = devices.get(&hb.device).cloned();
        let was_online = prev.as_ref().map(|d| d.online).unwrap_or(false);
        let was_healthy = prev.as_ref().map(|d| d.healthy).unwrap_or(true);

        devices.insert(
            hb.device.clone(),
            DeviceState {
                device: hb.device.clone(),
                service: hb.service.clone(),
                online: true,
                healthy: hb.healthy,
                consecutive_fails: hb.consecutive_fails,
                total_restarts: hb.total_restarts,
                last_seen: now(),
                note: hb.note.clone(),
            },
        );

        if !was_online {
            alerts.push((
                format!("[netdog] 🟢 {} 已上线", hb.device),
                format!("设备: {}\n服务: {}\n状态: 已连接到告警服务器\n时间: {}\n", hb.device, hb.service, stamp()),
            ));
        }
        if was_healthy && !hb.healthy {
            alerts.push((
                format!("[netdog] ⚠️ {} 代理异常", hb.device),
                format!("设备: {}\n服务: {}\n状态: 代理连通性异常 (设备在线)\n连续失败: {}\n备注: {}\n时间: {}\n", hb.device, hb.service, hb.consecutive_fails, hb.note, stamp()),
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
                    format!("设备: {}\n服务: {}\n状态: 失联 (超过 {}s 未收到心跳)\n最后心跳: {}s 前\n时间: {}\n\n该设备可能整机离线/断电/断网，无法自行发送告警，由服务端代为通知。", d.device, d.service, timeout, t - d.last_seen, stamp()),
                ));
            }
        }
    }
    for (s, b) in alerts {
        notify(app, s, b);
    }
}

// ---------------- assets ----------------

const STYLE: &str = "<style>\
:root{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;color:#1f2937}\
body{margin:0;background:#f3f4f6}\
header{display:flex;align-items:center;justify-content:space-between;padding:14px 24px;background:#111827;color:#fff}\
header h1{font-size:18px;margin:0}.sub{font-size:12px;color:#9ca3af;font-weight:400}\
.logout{color:#cbd5e1;text-decoration:none;font-size:14px}.logout:hover{color:#fff}\
.cards{display:flex;gap:16px;padding:20px 24px 0;flex-wrap:wrap}\
.card{background:#fff;border-radius:10px;padding:14px 20px;min-width:120px;box-shadow:0 1px 3px rgba(0,0,0,.08)}\
.card .n{font-size:28px;font-weight:700}.card .l{font-size:12px;color:#6b7280}\
table{width:calc(100% - 48px);margin:20px 24px;border-collapse:collapse;background:#fff;border-radius:10px;overflow:hidden;box-shadow:0 1px 3px rgba(0,0,0,.08)}\
th,td{padding:10px 14px;text-align:left;border-bottom:1px solid #f0f0f0;font-size:14px}\
th{background:#f9fafb;color:#6b7280;font-weight:600}\
.pill{padding:2px 10px;border-radius:10px;color:#fff;font-size:12px;font-weight:600}\
.foot{padding:0 24px 24px;color:#9ca3af;font-size:12px}\
.login{max-width:320px;margin:12vh auto;background:#fff;padding:32px;border-radius:12px;box-shadow:0 4px 16px rgba(0,0,0,.1)}\
.login h1{margin:0 0 20px;text-align:center}\
.login input{display:block;width:100%;box-sizing:border-box;margin:10px 0;padding:10px;border:1px solid #d1d5db;border-radius:8px;font-size:14px}\
.login button{width:100%;padding:10px;margin-top:10px;background:#111827;color:#fff;border:0;border-radius:8px;font-size:15px;cursor:pointer}\
.err{color:#dc2626;font-size:13px;text-align:center;margin:0 0 8px}\
</style>";

const SCRIPT: &str = "<script>\
function pill(d){let c='#16a34a',t='OK';if(!d.online){c='#dc2626';t='OFFLINE'}else if(!d.healthy){c='#d97706';t='UNHEALTHY'}return'<span class=pill style=\"background:'+c+'\">'+t+'</span>'}\
function esc(s){return(s||'').replace(/[&<>]/g,m=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[m]))}\
async function refresh(){let r;try{r=await fetch('/api/status')}catch(e){return}\
if(r.status===401){location='/login';return}\
let d=await r.json(),now=Math.floor(Date.now()/1000);\
let on=d.filter(x=>x.online&&x.healthy).length,off=d.filter(x=>!x.online).length,bad=d.filter(x=>x.online&&!x.healthy).length;\
document.getElementById('summary').innerHTML=\
'<div class=card><div class=n>'+d.length+'</div><div class=l>Devices</div></div>'+\
'<div class=card><div class=n style=color:#16a34a>'+on+'</div><div class=l>Healthy</div></div>'+\
'<div class=card><div class=n style=color:#d97706>'+bad+'</div><div class=l>Unhealthy</div></div>'+\
'<div class=card><div class=n style=color:#dc2626>'+off+'</div><div class=l>Offline</div></div>';\
document.getElementById('rows').innerHTML=d.length?d.map(x=>'<tr><td>'+esc(x.device)+'</td><td>'+esc(x.service)+'</td><td>'+pill(x)+'</td><td>'+x.consecutive_fails+'</td><td>'+x.total_restarts+'</td><td>'+(now-x.last_seen)+'s ago</td><td>'+esc(x.note)+'</td></tr>').join(''):'<tr><td colspan=7>No agents connected yet.</td></tr>';\
document.getElementById('ts').textContent=new Date().toLocaleTimeString();}\
refresh();setInterval(refresh,5000);\
</script>";
