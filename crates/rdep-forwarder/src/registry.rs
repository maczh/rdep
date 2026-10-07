use std::collections::HashMap;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::server::TlsStream;

/// 已注册 service 的隧道类型：service 拨号到 forwarder 后的 TLS 连接（已解密的明文流）。
pub type ServiceStream = TlsStream<TcpStream>;

/// 单个 service 可同时停放的隧道数上限（防止异常情况下无限堆积）。
const MAX_PARKED_PER_SERVICE: usize = 16;

/// 在线 service 注册表：`service_id → 停放中的隧道连接池`。
///
/// service 侧会维持多条（`RDEP_MAX_SESSIONS`，默认 4）常驻注册连接；每条被一个
/// client 会话取走配对后，其余仍停放待用，因此**同一 service 可并发服务多个 client**
/// （上限 = 连接池大小）。一个 client 会话结束后，其对应的 service 连接断开，
/// service 侧自动重连补回池中，维持并发能力。
pub struct Registry {
    map: Mutex<HashMap<String, Vec<ServiceStream>>>,
    token: String,
}

impl Registry {
    pub fn new(token: String) -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(HashMap::new()),
            token,
        })
    }

    /// 校验中转密钥。
    /// 常量时间比较中转密钥。
    ///
    /// 不能用 `==`：字符串比较在首个不同字节处短路，会泄漏前缀匹配长度的
    /// 时序信息。中转密钥是公网中继的唯一共享秘密，必须常量时间比较
    /// （与 service 侧 `password::verify` 的纪律保持一致）。
    pub fn check_token(&self, token: &str) -> bool {
        crate::password::ct_eq(token.as_bytes(), self.token.as_bytes())
    }

    /// 停放一条已注册的 service 隧道（同 id 累积；超过上限丢弃最旧的一条）。
    pub async fn register(&self, id: String, stream: ServiceStream) {
        let mut map = self.map.lock().await;
        let v = map.entry(id).or_default();
        v.push(stream);
        if v.len() > MAX_PARKED_PER_SERVICE {
            v.remove(0);
        }
    }

    /// 为某个 client 会话取出一条（并移出）可用的 service 隧道；无可用则返回 None。
    pub async fn take(&self, id: &str) -> Option<ServiceStream> {
        let mut map = self.map.lock().await;
        let v = map.get_mut(id)?;
        let s = v.pop();
        if v.is_empty() {
            map.remove(id);
        }
        s
    }

    /// 当前在线（至少有一条停放隧道）的 service id 列表。
    pub async fn online_ids(&self) -> Vec<String> {
        self.map
            .lock()
            .await
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| k.clone())
            .collect()
    }
}
