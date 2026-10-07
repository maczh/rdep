use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::password;

/// 当前 Unix 秒。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 账户与项目元数据存储（SQLite）。
///
/// Phase 1 仅使用 `users` 表做 AUTH；`projects` 表预留给后续发布/回滚。
/// 默认账户 `admin/admin`（密码以 sha256 十六进制存储，仅用于开发期）。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (\
                id INTEGER PRIMARY KEY,\
                username TEXT UNIQUE NOT NULL,\
                pass_hash TEXT NOT NULL,\
                method TEXT NOT NULL DEFAULT 'password'\
            );\
            CREATE TABLE IF NOT EXISTS projects (\
                id INTEGER PRIMARY KEY,\
                name TEXT NOT NULL,\
                remote_dir TEXT NOT NULL,\
                restart_script TEXT\
            );\
            CREATE TABLE IF NOT EXISTS api_tokens (\
                id INTEGER PRIMARY KEY,\
                token_hash TEXT UNIQUE NOT NULL,\
                username TEXT NOT NULL,\
                label TEXT NOT NULL DEFAULT '',\
                created_at INTEGER NOT NULL,\
                last_used_at INTEGER NOT NULL DEFAULT 0,\
                revoked INTEGER NOT NULL DEFAULT 0\
            );",
        )?;

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        if count == 0 {
            let h = password::hash_password("admin");
            conn.execute(
                "INSERT INTO users(username, pass_hash, method) VALUES(?1, ?2, 'password')",
                ["admin", &h],
            )?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 校验用户名/口令；成功返回 true。
    ///
    /// 口令散列使用 PBKDF2（见 `password` 模块）。为兼容**升级前**创建的
    /// 账户，`password::verify` 同时接受旧的裸 SHA-256 格式；认证成功且
    /// 存储值仍是旧格式时，就地**透明升级**为 PBKDF2 —— 部署升级后用户下一次
    /// 登录即生效，无需强制改密。
    pub fn auth(&self, user: &str, pass: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT pass_hash FROM users WHERE username=?1",
                [user],
                |r| r.get(0),
            )
            .ok();
        let Some(stored) = stored else {
            return false;
        };
        if !password::verify(&stored, pass) {
            return false;
        }
        if password::needs_rehash(&stored) {
            let upgraded = password::hash_password(pass);
            let _ = conn.execute(
                "UPDATE users SET pass_hash=?1 WHERE username=?2",
                rusqlite::params![upgraded, user],
            );
        }
        true
    }

    /// 异步认证：把 **CPU 密集的 KDF 派发到阻塞线程池**。
    ///
    /// PBKDF2 是刻意设计的慢函数（210k 轮 ≈ 100ms，debug 构建更久）。
    /// 若在 async 会话处理器里**同步**调用，会独占一个 tokio worker 线程，
    /// 足以让并发认证把整个 runtime 拖垮——这本身就是 DoS 向量。
    /// 因此这里只在锁内快速取出散列，耗时的 derive/比较交给 `spawn_blocking`。
    pub async fn auth_async(self: &Arc<Self>, user: &str, pass: &str) -> bool {
        // 快路径：只读一次散列，随即释放锁
        let stored: Option<String> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT pass_hash FROM users WHERE username=?1",
                [user],
                |r| r.get(0),
            )
            .ok()
        };
        let Some(stored) = stored else {
            return false;
        };
        let user = user.to_string();
        let pass = pass.to_string();
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            if !password::verify(&stored, &pass) {
                return false;
            }
            if password::needs_rehash(&stored) {
                let upgraded = password::hash_password(&pass);
                let conn = this.conn.lock().unwrap();
                let _ = conn.execute(
                    "UPDATE users SET pass_hash=?1 WHERE username=?2",
                    rusqlite::params![upgraded, user],
                );
            }
            true
        })
        .await
        .unwrap_or(false)
    }

    // ---- API 令牌（供 CI/CD 用口令之外的凭据） ----

    /// 创建 API 令牌，返回 `(id, 明文令牌)`。
    ///
    /// 明文**只在创建时返回一次**，库里仅存 SHA-256 摘要。
    /// 令牌是 256 bit 随机值（高熵），故用快速哈希即可——这与口令必须用
    /// PBKDF2 的理由不同：PBKDF2 是为了抬高**低熵人类口令**的猜测成本。
    pub fn create_token(&self, username: &str, label: &str) -> Result<(i64, String)> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| anyhow::anyhow!("generate token failed: {e}"))?;
        let plain = format!(
            "rdp_{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let hash = token_hash(&plain);
        let now = now_secs();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO api_tokens(token_hash, username, label, created_at) VALUES(?1,?2,?3,?4)",
            rusqlite::params![hash, username, label, now],
        )?;
        let id = conn.last_insert_rowid();
        Ok((id, plain))
    }

    /// 列出令牌（**不含**明文，仅元数据）。
    pub fn list_tokens(&self) -> Result<Vec<TokenRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, username, label, created_at, last_used_at, revoked \
             FROM api_tokens ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TokenRow {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    label: r.get(2)?,
                    created_at: r.get(3)?,
                    last_used_at: r.get(4)?,
                    revoked: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 吊销令牌；返回是否确实存在且被更新。
    pub fn revoke_token(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("UPDATE api_tokens SET revoked=1 WHERE id=?1", [id])?;
        Ok(n > 0)
    }

    /// 校验令牌：有效则返回所属用户名并更新 `last_used_at`。
    pub fn verify_token(&self, plain: &str) -> Result<Option<String>> {
        let hash = token_hash(plain);
        let conn = self.conn.lock().unwrap();
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, username FROM api_tokens WHERE token_hash=?1 AND revoked=0",
                [&hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((id, username)) = row else {
            return Ok(None);
        };
        let _ = conn.execute(
            "UPDATE api_tokens SET last_used_at=?1 WHERE id=?2",
            rusqlite::params![now_secs(), id],
        );
        Ok(Some(username))
    }

    // ---- 管理后台（Web）用到的用户 / 项目 CRUD ----

    /// 列出所有用户（id, username）。
    pub fn list_users(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, username FROM users ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 新建用户（用户名唯一）。
    pub fn create_user(&self, username: &str, pass: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let h = password::hash_password(pass);
        conn.execute(
            "INSERT INTO users(username, pass_hash, method) VALUES(?1, ?2, 'password')",
            rusqlite::params![username, h],
        )?;
        Ok(())
    }

    /// 按 id 删除用户。
    pub fn delete_user(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM users WHERE id=?1", [id])?;
        Ok(())
    }

    /// 列出所有项目。
    /// 按名称查找项目（发布时用于把「项目」解析为部署配置）。
    pub fn find_project(&self, name: &str) -> Result<Option<ProjectRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, remote_dir, COALESCE(restart_script,'') FROM projects WHERE name=?1",
        )?;
        let mut rows = stmt.query_map([name], |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                name: r.get(1)?,
                remote_dir: r.get(2)?,
                restart_script: r.get(3)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, remote_dir, COALESCE(restart_script,'') FROM projects ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProjectRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    remote_dir: r.get(2)?,
                    restart_script: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 新建项目。
    pub fn create_project(&self, name: &str, remote_dir: &str, restart_script: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO projects(name, remote_dir, restart_script) VALUES(?1, ?2, ?3)",
            rusqlite::params![name, remote_dir, restart_script],
        )?;
        Ok(())
    }

    /// 按 id 删除项目。
    pub fn delete_project(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM projects WHERE id=?1", [id])?;
        Ok(())
    }
}

/// 一个项目记录。
/// 一条 API 令牌的元数据（不含明文）。
#[derive(Debug, Clone, Serialize)]
pub struct TokenRow {
    pub id: i64,
    pub username: String,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: i64,
    pub revoked: bool,
}

/// 令牌摘要（明文只在创建时出现一次）。
pub fn token_hash(plain: &str) -> String {
    let mut h = Sha256::new();
    h.update(plain.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub struct ProjectRow {
    pub id: i64,
    pub name: String,
    pub remote_dir: String,
    pub restart_script: String,
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_db(tag: &str) -> (Db, PathBuf) {
        let p = std::env::temp_dir().join(format!("rdep-auth-{}-{}.db", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        (Db::open(&p).expect("open db"), p)
    }

    fn stored_hash(db: &Db, user: &str) -> String {
        let conn = db.conn.lock().unwrap();
        conn.query_row("SELECT pass_hash FROM users WHERE username=?1", [user], |r| r.get(0))
            .expect("hash row")
    }

    /// 新建库的默认账户用 PBKDF2 存储（不再是裸 SHA-256）。
    #[test]
    fn default_admin_uses_pbkdf2() {
        let (db, p) = tmp_db("new");
        let h = stored_hash(&db, "admin");
        assert!(
            h.starts_with("pbkdf2-sha256$"),
            "默认账户应使用 PBKDF2，实际: {h}"
        );
        assert!(db.auth("admin", "admin"));
        assert!(!db.auth("admin", "wrong"));
        let _ = std::fs::remove_file(&p);
    }

    /// 升级兼容：旧库里的裸 SHA-256 口令仍能登录，且登录后**自动升级**为 PBKDF2。
    #[test]
    fn legacy_hash_is_migrated_on_login() {
        use sha2::{Digest, Sha256};
        let (db, p) = tmp_db("migrate");

        // 模拟升级前的库：把 admin 的散列改回裸 SHA-256
        let legacy = {
            let mut h = Sha256::new();
            h.update(b"admin");
            format!("{:x}", h.finalize())
        };
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE users SET pass_hash=?1 WHERE username='admin'",
                [&legacy],
            )
            .unwrap();
        }
        assert_eq!(stored_hash(&db, "admin"), legacy, "前置条件：已是旧格式");

        // 旧账户仍可登录
        assert!(db.auth("admin", "admin"), "旧格式账户必须仍能登录");
        assert!(!db.auth("admin", "nope"), "错误口令仍应失败");

        // 登录成功后已透明升级
        let after = stored_hash(&db, "admin");
        assert!(
            after.starts_with("pbkdf2-sha256$"),
            "登录后应自动升级为 PBKDF2，实际: {after}"
        );
        assert_ne!(after, legacy);
        // 升级后仍可登录，且升级是幂等的
        assert!(db.auth("admin", "admin"));
        let after2 = stored_hash(&db, "admin");
        assert!(after2.starts_with("pbkdf2-sha256$"));
        assert!(!password::needs_rehash(&after2), "已是新格式，不应反复重写");

        let _ = std::fs::remove_file(&p);
    }

    /// 新建用户同样使用 PBKDF2，且同名用户校验正常。
    #[test]
    fn created_user_uses_pbkdf2() {
        let (db, p) = tmp_db("create");
        db.create_user("deployer", "pw12345").unwrap();
        let h = stored_hash(&db, "deployer");
        assert!(h.starts_with("pbkdf2-sha256$"), "实际: {h}");
        assert!(db.auth("deployer", "pw12345"));
        assert!(!db.auth("deployer", "pw12346"));
        // 相同口令的两个用户散列不同（加盐生效）
        db.create_user("deployer2", "pw12345").unwrap();
        assert_ne!(stored_hash(&db, "deployer"), stored_hash(&db, "deployer2"));
        // 不存在的用户
        assert!(!db.auth("ghost", "pw12345"));
        let _ = std::fs::remove_file(&p);
    }
}
