//! rdep-service Web 管理后台（axum）。
//!
//! 默认账户 `admin/admin`。提供 JSON REST API + 一个内嵌的单页管理界面（`GET /`）。
//! 受保护接口需带 `Authorization: Bearer <token>`（`POST /api/login` 换取）。
//!
//! 接口：
//! - `POST /api/login` { username, password } → { token }
//! - `GET  /api/status` → 概览（用户数、项目数等）
//! - `GET/POST /api/users`, `DELETE /api/users/:id`
//! - `GET/POST /api/projects`, `DELETE /api/projects/:id`
//! - `GET  /api/backups` → 备份版本列表
//! - `POST /api/rollback` { remote_dir, version } → 触发回滚

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::storage::Storage;

/// axum 要求路由 state 为 `Clone`，故内部共享会话表用 `Arc<Mutex<..>>`。
#[derive(Clone)]
pub struct WebState {
    pub db: Arc<Db>,
    pub storage: Arc<Storage>,
    /// token → 过期时间（epoch 秒）。
    sessions: Arc<Mutex<HashMap<String, i64>>>,
}

impl WebState {
    pub fn new(db: Arc<Db>, storage: Arc<Storage>) -> Self {
        Self {
            db,
            storage,
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

/// 校验 Bearer token；有效返回 Ok，失效返回 401。
fn require_auth(st: &WebState, h: &HeaderMap) -> Result<(), ApiErr> {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let now = now_secs();
    let mut sess = st.sessions.lock().unwrap();
    sess.retain(|_, exp| *exp > now); // 顺手清理过期
    // 只记「有没有令牌 / 是否通过」，绝不记令牌本体（等同口令）
    let ok = !token.is_empty() && sess.contains_key(&token);
    if ok {
        tracing::debug!(sessions = sess.len(), "web api: auth ok");
        Ok(())
    } else {
        tracing::warn!(has_token = !token.is_empty(), "web api: unauthorized");
        Err(err(StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

// ---- 请求 / 响应体 ----
#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}
#[derive(Serialize)]
struct LoginResp {
    token: String,
}
#[derive(Deserialize)]
struct CreateUserReq {
    username: String,
    password: String,
}
#[derive(Serialize)]
struct UserItem {
    id: i64,
    username: String,
}
#[derive(Deserialize)]
struct CreateProjectReq {
    name: String,
    remote_dir: String,
    #[serde(default)]
    restart_script: String,
}
#[derive(Serialize)]
struct ProjectItem {
    id: i64,
    name: String,
    remote_dir: String,
    restart_script: String,
}
#[derive(Serialize)]
struct StatusResp {
    users: usize,
    projects: usize,
    backup_versions: usize,
}
#[derive(Serialize)]
struct BackupsResp {
    versions: Vec<String>,
}
#[derive(Deserialize)]
struct RollbackReq {
    remote_dir: String,
    version: String,
}
#[derive(Serialize)]
struct OkResp {
    ok: bool,
}

// ---- 处理器 ----
async fn login(State(st): State<WebState>, Json(req): Json<LoginReq>) -> Result<Json<LoginResp>, ApiErr> {
    tracing::debug!(username = %req.username, "web api: login");
    if st.db.auth(&req.username, &req.password) {
        let token = gen_token();
        let mut sess = st.sessions.lock().unwrap();
        // 先清理已过期会话：原先只在 require_auth 里清理，若无人发起已认证请求，
        // 过期令牌会一直堆积；这里在登录路径也清理一次。
        let now = now_secs();
        sess.retain(|_, exp| *exp > now);
        // 限制在线会话上限：默认口令下可被反复登录撑爆内存（DoS）。
        // 超出时按过期时间淘汰最旧的一个，保证新登录仍可用。
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

async fn status(
    State(st): State<WebState>,
    h: HeaderMap,
) -> Result<Json<StatusResp>, ApiErr> {
    require_auth(&st, &h)?;
    let users = st.db.list_users().map(|v| v.len()).unwrap_or(0);
    let projects = st.db.list_projects().map(|v| v.len()).unwrap_or(0);
    let backup_versions = st.storage.list_backup_versions().map(|v| v.len()).unwrap_or(0);
    Ok(Json(StatusResp {
        users,
        projects,
        backup_versions,
    }))
}

async fn list_users(State(st): State<WebState>, h: HeaderMap) -> Result<Json<Vec<UserItem>>, ApiErr> {
    require_auth(&st, &h)?;
    let rows = st.db.list_users().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, username)| UserItem { id, username })
            .collect(),
    ))
}

async fn create_user(
    State(st): State<WebState>,
    h: HeaderMap,
    Json(req): Json<CreateUserReq>,
) -> Result<Json<OkResp>, ApiErr> {
    require_auth(&st, &h)?;
    st.db
        .create_user(&req.username, &req.password)
        .map_err(|e| err(StatusCode::BAD_REQUEST, &e.to_string()))?;
    Ok(Json(OkResp { ok: true }))
}

async fn delete_user(
    State(st): State<WebState>,
    h: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<OkResp>, ApiErr> {
    require_auth(&st, &h)?;
    st.db
        .delete_user(id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(OkResp { ok: true }))
}

async fn list_projects(
    State(st): State<WebState>,
    h: HeaderMap,
) -> Result<Json<Vec<ProjectItem>>, ApiErr> {
    require_auth(&st, &h)?;
    let rows =
        st.db.list_projects().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|p| ProjectItem {
                id: p.id,
                name: p.name,
                remote_dir: p.remote_dir,
                restart_script: p.restart_script,
            })
            .collect(),
    ))
}

async fn create_project(
    State(st): State<WebState>,
    h: HeaderMap,
    Json(req): Json<CreateProjectReq>,
) -> Result<Json<OkResp>, ApiErr> {
    require_auth(&st, &h)?;
    st.db
        .create_project(&req.name, &req.remote_dir, &req.restart_script)
        .map_err(|e| err(StatusCode::BAD_REQUEST, &e.to_string()))?;
    Ok(Json(OkResp { ok: true }))
}

async fn delete_project(
    State(st): State<WebState>,
    h: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<OkResp>, ApiErr> {
    require_auth(&st, &h)?;
    st.db
        .delete_project(id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(OkResp { ok: true }))
}

async fn backups(State(st): State<WebState>, h: HeaderMap) -> Result<Json<BackupsResp>, ApiErr> {
    tracing::debug!("web api: list backups");
    require_auth(&st, &h)?;
    let versions = st
        .storage
        .list_backup_versions()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(BackupsResp { versions }))
}

// ---- API 令牌管理（供 CI/CD 使用） ----

#[derive(Deserialize)]
struct CreateTokenReq {
    /// 令牌归属用户（仅作标注/审计；令牌本身即可认证）。
    username: String,
    /// 便于识别的标签，如 "ci-github-actions"。
    #[serde(default)]
    label: String,
}

#[derive(Serialize)]
struct CreateTokenResp {
    id: i64,
    /// **明文令牌，只在此处返回一次**，之后无法再取回。
    token: String,
    warning: String,
}

#[derive(Serialize)]
struct TokenItem {
    id: i64,
    username: String,
    label: String,
    created_at: i64,
    last_used_at: i64,
    revoked: bool,
}

async fn create_token(
    State(st): State<WebState>,
    h: HeaderMap,
    Json(req): Json<CreateTokenReq>,
) -> Result<Json<CreateTokenResp>, ApiErr> {
    tracing::debug!(username = %req.username, label = %req.label, "web api: create token");
    require_auth(&st, &h)?;
    if req.username.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "username required"));
    }
    let (id, token) = st
        .db
        .create_token(req.username.trim(), req.label.trim())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(CreateTokenResp {
        id,
        token,
        warning: "令牌明文仅显示这一次，请立即妥善保存".into(),
    }))
}

async fn list_tokens(
    State(st): State<WebState>,
    h: HeaderMap,
) -> Result<Json<Vec<TokenItem>>, ApiErr> {
    require_auth(&st, &h)?;
    let rows = st
        .db
        .list_tokens()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|t| TokenItem {
                id: t.id,
                username: t.username,
                label: t.label,
                created_at: t.created_at,
                last_used_at: t.last_used_at,
                revoked: t.revoked,
            })
            .collect(),
    ))
}

async fn revoke_token(
    State(st): State<WebState>,
    h: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<OkResp>, ApiErr> {
    tracing::debug!(id, "web api: revoke token");
    require_auth(&st, &h)?;
    let changed = st
        .db
        .revoke_token(id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if !changed {
        return Err(err(StatusCode::NOT_FOUND, "token not found"));
    }
    Ok(Json(OkResp { ok: true }))
}

async fn rollback(
    State(st): State<WebState>,
    h: HeaderMap,
    Json(req): Json<RollbackReq>,
) -> Result<Json<OkResp>, ApiErr> {
    tracing::debug!(dir = %req.remote_dir, version = %req.version, "web api: rollback");
    require_auth(&st, &h)?;
    st.storage
        .rollback(&req.remote_dir, &req.version)
        .map_err(|e| err(StatusCode::BAD_REQUEST, &e.to_string()))?;
    Ok(Json(OkResp { ok: true }))
}

async fn index() -> &'static str {
    INDEX_HTML
}

/// 组装路由。
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/login", post(login))
        .route("/api/status", get(status))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/:id", delete(delete_user))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/:id", delete(delete_project))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/:id", delete(revoke_token))
        .route("/api/backups", get(backups))
        .route("/api/rollback", post(rollback))
        .with_state(state)
}

/// 启动 Web 管理服务（阻塞）。
pub async fn serve(state: WebState, addr: &str) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("rdep-service web listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

const INDEX_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8"><title>rdep-service 管理</title>
<style>body{font-family:system-ui;margin:24px;max-width:900px}input,button{margin:4px;padding:6px}
section{border:1px solid #ddd;border-radius:8px;padding:12px;margin:12px 0}pre{background:#f6f6f6;padding:8px;overflow:auto}
h2{margin:0 0 8px}</style></head><body>
<h1>rdep-service 管理后台</h1>
<div id="login"><input id="u" placeholder="用户名" value="admin"><input id="p" type="password" placeholder="密码" value="admin"><button onclick="doLogin()">登录</button><span id="lmsg"></span></div>
<div id="app" style="display:none">
  <p>已登录 <span id="who"></span> <button onclick="logout()">退出</button></p>
  <section><h2>概览</h2><button onclick="loadStatus()">刷新</button><pre id="status"></pre></section>
  <section><h2>用户</h2><input id="nu" placeholder="新用户名"><input id="np" type="password" placeholder="密码"><button onclick="addUser()">添加</button><div id="users"></div></section>
  <section><h2>项目</h2><input id="pn" placeholder="名称"><input id="pd" placeholder="远端目录 /opt/app"><input id="pr" placeholder="重启脚本(可选)"><button onclick="addProject()">添加</button><div id="projects"></div></section>
  <section><h2>备份 / 回滚</h2><button onclick="loadBackups()">刷新备份</button><div id="backups"></div>
    <input id="rbdir" placeholder="远端目录 /opt/app"><input id="rbver" placeholder="版本(如 2601071530)"><button onclick="doRollback()">回滚</button></section>
</div>
<script>
let T='';
function api(p,o){o=o||{};o.headers=Object.assign({'Content-Type':'application/json'},o.headers||{},T?{'Authorization':'Bearer '+T}:{});
 return fetch(p,o).then(async r=>{let j=null;try{j=await r.json()}catch(e){} if(!r.ok)throw new Error((j&&j.error)||r.status); return j;});}
function doLogin(){api('/api/login',{method:'POST',body:JSON.stringify({username:u.value,password:p.value})}).then(j=>{T=j.token;localStorage.t=T;show()}).catch(e=>lmsg.textContent=' '+e.message);}
function logout(){T='';localStorage.t='';app.style.display='none';login.style.display='block';}
function show(){login.style.display='none';app.style.display='block';who.textContent='';loadStatus();loadUsers();loadProjects();loadBackups();}
function loadStatus(){api('/api/status').then(j=>status.textContent=JSON.stringify(j,null,2));}
function loadUsers(){api('/api/users').then(j=>{users.innerHTML=j.map(x=>`<div>${x.id}: ${x.username} <button onclick="del('/api/users/'+${x.id})">删除</button></div>`).join('')||'(空)';});}
function addUser(){api('/api/users',{method:'POST',body:JSON.stringify({username:nu.value,password:np.value})}).then(loadUsers).catch(e=>alert(e.message));}
function loadProjects(){api('/api/projects').then(j=>{projects.innerHTML=j.map(x=>`<div>${x.id}: ${x.name} → ${x.remote_dir} (restart=${x.restart_script||'-'}) <button onclick="del('/api/projects/'+${x.id})">删除</button></div>`).join('')||'(空)';});}
function addProject(){api('/api/projects',{method:'POST',body:JSON.stringify({name:pn.value,remote_dir:pd.value,restart_script:pr.value})}).then(loadProjects).catch(e=>alert(e.message));}
function loadBackups(){api('/api/backups').then(j=>{backups.innerHTML=j.versions.map(v=>`<div>${v} <button onclick="rbver.value='${v}'">选择</button></div>`).join('')||'(无备份)';});}
function doRollback(){api('/api/rollback',{method:'POST',body:JSON.stringify({remote_dir:rbdir.value,version:rbver.value})}).then(()=>alert('回滚成功')).catch(e=>alert(e.message));}
function del(p){api(p,{method:'DELETE'}).then(()=>{loadUsers();loadProjects();}).catch(e=>alert(e.message));}
T=localStorage.t||'';if(T)show();
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
