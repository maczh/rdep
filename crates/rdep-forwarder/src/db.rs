use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;

use crate::password;
use rusqlite::Connection;

/// 中转审计与账户存储（SQLite）。
///
/// `services` 表记录注册过的 service（id/label/last_seen，供管理界面与审计）；
/// `users` 表预留给管理后台登录（默认 `admin/admin`，密码以 sha256 十六进制存储）。
pub struct Db {
    conn: Mutex<Connection>,
}

/// 一条 service 记录。
pub struct ServiceRow {
    pub id: String,
    pub label: String,
    pub last_seen: i64,
    /// 累计被中继服务的客户端会话数。
    pub client_sessions: i64,
    /// 最后一次服务客户端会话的时间（epoch 秒，0 = 从未）。
    pub last_client_at: i64,
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
            CREATE TABLE IF NOT EXISTS services (\
                id TEXT PRIMARY KEY,\
                label TEXT NOT NULL DEFAULT '',\
                last_seen INTEGER NOT NULL DEFAULT 0,\
                client_sessions INTEGER NOT NULL DEFAULT 0,\
                last_client_at INTEGER NOT NULL DEFAULT 0\
            );",
        )?;

        // 迁移：老库补列（CREATE TABLE IF NOT EXISTS 不会给已存在的表加列）
        for (col, ddl) in [
            ("client_sessions", "ALTER TABLE services ADD COLUMN client_sessions INTEGER NOT NULL DEFAULT 0"),
            ("last_client_at", "ALTER TABLE services ADD COLUMN last_client_at INTEGER NOT NULL DEFAULT 0"),
        ] {
            let has: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('services') WHERE name=?1",
                [col],
                |r| r.get(0),
            )?;
            if has == 0 {
                conn.execute(ddl, [])?;
            }
        }

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

    /// 记录一次**客户端中转会话**：累加计数并更新最后服务时间。
    ///
    /// 与 `touch_service`（注册时刻）区分开：中继的在线状态看内存注册表，
    /// 而这里回答的是「这个 service 到底被谁用过、用了多少次」。
    pub fn record_client_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_secs();
        // 服务可能尚未注册（极少见），故用 ON CONFLICT 保证不丢事件。
        // 注意：不要用 `\` 续行写 SQL —— Rust 会吞掉下一行前导空白，
        // 把 `SET` 和 `client_sessions` 粘成 `SETclient_sessions` 导致语法错误。
        conn.execute(
            "INSERT INTO services(id, label, last_seen, client_sessions, last_client_at)              VALUES(?1, '', 0, 1, ?2)              ON CONFLICT(id) DO UPDATE SET                client_sessions = client_sessions + 1,                last_client_at = ?2",
            rusqlite::params![id, now],
        )?;
        Ok(())
    }

    /// service 注册时 upsert（更新 label 与 last_seen）。
    pub fn touch_service(&self, id: &str, label: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_secs();
        conn.execute(
            "INSERT INTO services(id, label, last_seen) VALUES(?1, ?2, ?3)\
             ON CONFLICT(id) DO UPDATE SET label=?2, last_seen=?3",
            rusqlite::params![id, label, now],
        )?;
        Ok(())
    }

    /// 列出所有已知 service（按 id 排序）。
    pub fn list_services(&self) -> Result<Vec<ServiceRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, label, last_seen, client_sessions, last_client_at FROM services ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ServiceRow {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    last_seen: r.get(2)?,
                    client_sessions: r.get(3)?,
                    last_client_at: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 校验用户名/密码（管理后台用）；成功返回 true。
    /// 校验用户名/口令；成功返回 true。
    ///
    /// 口令散列使用 PBKDF2（见 `password` 模块），并兼容旧的裸 SHA-256 格式；
    /// 认证成功且仍是旧格式时**透明升级**，部署升级后下次登录即生效。
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
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_db(tag: &str) -> (Db, PathBuf) {
        let p = std::env::temp_dir().join(format!("rdep-fwd-{}-{}.db", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        (Db::open(&p).expect("open db"), p)
    }

    /// 中继审计：注册后无会话；每次客户端中转计数 +1 并更新 last_client_at。
    #[test]
    fn client_session_audit() {
        let (db, p) = tmp_db("audit");

        db.touch_service("svc-a", "label-a").unwrap();
        let rows = db.list_services().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].client_sessions, 0, "刚注册不应有会话");
        assert_eq!(rows[0].last_client_at, 0);

        db.record_client_session("svc-a").unwrap();
        db.record_client_session("svc-a").unwrap();
        let rows = db.list_services().unwrap();
        assert_eq!(rows[0].client_sessions, 2, "两次中转应计为 2");
        assert!(rows[0].last_client_at > 0, "last_client_at 应被更新");
        // 原有字段不应被破坏
        assert_eq!(rows[0].id, "svc-a");
        assert_eq!(rows[0].label, "label-a");
        assert!(rows[0].last_seen > 0);

        // 未注册过的 id 也应被记录（ON CONFLICT 保证不丢事件）
        db.record_client_session("svc-unknown").unwrap();
        let rows = db.list_services().unwrap();
        let u = rows.iter().find(|r| r.id == "svc-unknown").expect("unknown recorded");
        assert_eq!(u.client_sessions, 1);

        let _ = std::fs::remove_file(&p);
    }

    /// 老库迁移：不含新列的 services 表应被补齐，且历史行保留。
    #[test]
    fn migrates_legacy_services_table() {
        use rusqlite::Connection;
        let p = std::env::temp_dir().join(format!("rdep-fwd-legacy-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        // 手工建一个「旧版」表
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "CREATE TABLE services (id TEXT PRIMARY KEY, label TEXT NOT NULL DEFAULT '', last_seen INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO services(id,label,last_seen) VALUES('old','legacy',12345);",
            )
            .unwrap();
        }
        let db = Db::open(&p).expect("open legacy db should migrate");
        db.record_client_session("old").unwrap();
        let rows = db.list_services().unwrap();
        let r = rows.iter().find(|r| r.id == "old").expect("legacy row kept");
        assert_eq!(r.label, "legacy", "历史数据应保留");
        assert_eq!(r.last_seen, 12345);
        assert_eq!(r.client_sessions, 1, "迁移后新列可用");
        let _ = std::fs::remove_file(&p);
    }
}
