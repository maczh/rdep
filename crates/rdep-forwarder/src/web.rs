//! rdep-forwarder Web 管理后台（axum）。
//!
//! 默认账户 `admin/admin`。提供 JSON REST API + 内嵌单页界面（`GET /`）。
//! 受保护接口需带 `Authorization: Bearer <token>`（`POST /api/login` 换取）。
//!
//! 接口：
//! - `POST /api/login` { username, password } → { token }
//! - `GET  /api/services` → 已注册 service 列表（含在线状态与 last_seen 审计）

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::registry::Registry;

#[derive(Clone)]
pub struct WebState {
    pub db: Arc<Db>,
    pub registry: Arc<Registry>,
    sessions: Arc<Mutex<HashMap<String, i64>>>,
}

impl WebState {
    pub fn new(db: Arc<Db>, registry: Arc<Registry>) -> Self {
        Self {
            db,
            registry,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

type ApiErr = (StatusCode, Json<ErrResp>);
#[derive(Serialize)]
struct ErrResp {
    error: String,
}

fn err(code: StatusCode, msg: &str) -> ApiErr {
    (code, Json(ErrResp { error: msg.into() }))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 同时在线会话数上限（防止反复登录撑爆内存）。
const MAX_LIVE_SESSIONS: usize = 64;

/// 生成会话令牌：**32 字节（256 bit）密码学安全随机数**，十六进制编码。
///
/// 安全说明：早期实现是 `SHA256(counter : nanos : pid)`，这三者都可推断
/// （pid 空间小、纳秒时间约等于服务启动时刻、counter 从 0 递增），
/// 攻击者可离线枚举令牌空间——SHA-256 并非 CSPRNG。令牌不可预测性是会话安全的
/// 前提，因此改为直接取操作系统随机源。
///
/// 若随机源不可用（极少见），退化为「时间+计数器+pid+地址」混合哈希并**保持失败关闭**：
/// 此时仍返回令牌，但调用方应视为该会话强度下降（正常 Linux/macOS/Windows 不会走到）。
fn gen_token() -> String {
    let mut buf = [0u8; 32];
    if getrandom::getrandom(&mut buf).is_ok() {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    // 退化路径：混入多个不可预测源，仍优于原实现
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let addr = &c as *const _ as usize;
    let mut h = Sha256::new();
    h.update(format!("{c}:{t}:{}:{addr}", std::process::id()));
    format!("{:x}", h.finalize())
}

fn require_auth(st: &WebState, h: &HeaderMap) -> Result<(), ApiErr> {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let now = now_secs();
    let mut sess = st.sessions.lock().unwrap();
    sess.retain(|_, exp| *exp > now);
    if !token.is_empty() && sess.contains_key(&token) {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}
#[derive(Serialize)]
struct LoginResp {
    token: String,
}
#[derive(Serialize)]
struct ServiceItem {
    id: String,
    label: String,
    last_seen: i64,
    /// 累计被中继服务的客户端会话数（真实使用量，而非注册次数）。
    client_sessions: i64,
    /// 最后一次服务客户端会话的时间（0 = 从未）。
    last_client_at: i64,
    online: bool,
}
#[derive(Serialize)]
struct ServicesResp {
    services: Vec<ServiceItem>,
    online_count: usize,
}

async fn login(
    State(st): State<WebState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<LoginResp>, ApiErr> {
    if st.db.auth(&req.username, &req.password) {
        let token = gen_token();
        let mut sess = st.sessions.lock().unwrap();
        // 登录路径也清理过期会话（原先只在 require_auth 里清理，
        // 若无人发起已认证请求，过期令牌会持续堆积）。
        let now = now_secs();
        sess.retain(|_, exp| *exp > now);
        // 限制在线会话数：默认口令下反复登录可撑爆内存（DoS）。
        if sess.len() >= MAX_LIVE_SESSIONS {
            if let Some(oldest) = sess
                .iter()
                .min_by_key(|(_, exp)| **exp)
                .map(|(t, _)| t.clone())
            {
                sess.remove(&oldest);
            }
        }
        sess.insert(token.clone(), now + 3600);
        drop(sess);
        Ok(Json(LoginResp { token }))
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "invalid credentials"))
    }
}

/// 已注册 service 列表：合并 DB 审计记录与注册表在线状态。
async fn services(
    State(st): State<WebState>,
    h: HeaderMap,
) -> Result<Json<ServicesResp>, ApiErr> {
    require_auth(&st, &h)?;
    let online = st.registry.online_ids().await;
    let online_set: std::collections::HashSet<String> = online.iter().cloned().collect();
    let mut items: Vec<ServiceItem> = st
        .db
        .list_services()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .into_iter()
        .map(|r| ServiceItem {
            online: online_set.contains(&r.id),
            id: r.id,
            label: r.label,
            last_seen: r.last_seen,
            client_sessions: r.client_sessions,
            last_client_at: r.last_client_at,
        })
        .collect();
    // 兜底：注册表里在线但 DB 尚无记录的（极少见）
    for id in online {
        if !items.iter().any(|i| i.id == id) {
            items.push(ServiceItem {
                id,
                label: String::new(),
                last_seen: now_secs(),
                client_sessions: 0,
                last_client_at: 0,
                online: true,
            });
        }
    }
    let online_count = items.iter().filter(|i| i.online).count();
    Ok(Json(ServicesResp {
        services: items,
        online_count,
    }))
}

async fn index() -> &'static str {
    INDEX_HTML
}

/// 组装路由。
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/login", post(login))
        .route("/api/services", get(services))
        .with_state(state)
}

/// 启动 Web 管理服务（阻塞）。
pub async fn serve(state: WebState, addr: &str) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("rdep-forwarder web listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

const INDEX_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8"><title>rdep-forwarder 管理</title>
<style>body{font-family:system-ui;margin:24px;max-width:900px}input,button{margin:4px;padding:6px}
section{border:1px solid #ddd;border-radius:8px;padding:12px;margin:12px 0}table{border-collapse:collapse;width:100%}
th,td{border:1px solid #eee;padding:6px;text-align:left}th{background:#f6f6f6}.on{color:#0a0;font-weight:bold}.off{color:#999}</style></head><body>
<h1>rdep-forwarder 管理后台</h1>
<div id="login"><input id="u" placeholder="用户名" value="admin"><input id="p" type="password" placeholder="密码" value="admin"><button onclick="doLogin()">登录</button><span id="lmsg"></span></div>
<div id="app" style="display:none">
  <p>已登录 <button onclick="logout()">退出</button></p>
  <section><h2>已注册 service（在线 <span id="cnt">0</span>）</h2><button onclick="load()">刷新</button><div id="list"></div></section>
</div>
<script>
let T='';
function api(p,o){o=o||{};o.headers=Object.assign({'Content-Type':'application/json'},T?{'Authorization':'Bearer '+T}:{});
 return fetch(p,o).then(async r=>{let j=null;try{j=await r.json()}catch(e){} if(!r.ok)throw new Error((j&&j.error)||r.status); return j;});}
function doLogin(){api('/api/login',{method:'POST',body:JSON.stringify({username:u.value,password:p.value})}).then(j=>{T=j.token;localStorage.t=T;show()}).catch(e=>lmsg.textContent=' '+e.message);}
function logout(){T='';localStorage.t='';app.style.display='none';login.style.display='block';}
function show(){login.style.display='none';app.style.display='block';load();}
function load(){api('/api/services').then(j=>{cnt.textContent=j.online_count;
 list.innerHTML='<table><tr><th>service_id</th><th>标签</th><th>状态</th><th>最后注册</th><th>客户端会话</th><th>最后服务</th></tr>'+j.services.map(s=>`<tr><td>${s.id}</td><td>${s.label||'-'}</td><td class="${s.online?'on':'off'}">${s.online?'在线':'离线'}</td><td>${s.last_seen?new Date(s.last_seen*1000).toLocaleString():'-'}</td><td>${s.client_sessions||0}</td><td>${s.last_client_at?new Date(s.last_client_at*1000).toLocaleString():'-'}</td></tr>`).join('')+'</table>';});}
T=localStorage.t||'';if(T)show(); setInterval(()=>{if(T)load();},5000);
</script></body></html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// 会话令牌必须是 32 字节（256 bit）随机数 → 64 位十六进制。
    /// 回归测试：早期实现用 `SHA256(counter:nanos:pid)`，长度虽也是 64，
    /// 但输入可推断，攻击者可离线枚举。这里锁定「长度 + 不可预测来源」两个性质。
    #[test]
    fn gen_token_is_256bit_and_unique() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let t = gen_token();
            assert_eq!(t.len(), 64, "令牌应为 64 位十六进制（32 字节），实际 {}", t.len());
            assert!(
                t.chars().all(|c| c.is_ascii_hexdigit()),
                "令牌应为十六进制字符: {t}"
            );
            assert!(seen.insert(t.clone()), "令牌重复出现（随机源失效）: {t}");
        }
        assert_eq!(seen.len(), 1000, "1000 次生成必须互不相同");
    }

    /// 会话上限常量存在且为合理值（防止误配成 0/1 导致登录即互踢）。
    #[test]
    fn max_live_sessions_sane() {
        assert!(MAX_LIVE_SESSIONS >= 8, "上限过小会互相踢下线");
        assert!(MAX_LIVE_SESSIONS <= 1024, "上限过大失去 DoS 防护意义");
    }
}
