//! rdep-forwarder 中转握手消息（service ⇄ forwarder、client ⇄ forwarder）。
//!
//! 这些消息只在「连接建立的第一帧」使用；握手完成后 forwarder 转为
//! **零解析字节透传**（不解释 rdep 协议），保证私有协议攻击面不扩大。

use serde::{Deserialize, Serialize};

/// service → forwarder：注册（建立一条常驻隧道连接）。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RelayHello {
    pub service_id: String,
    /// 便于在管理界面展示的标签。
    pub label: String,
    /// 共享密钥（与 forwarder 配置一致）。
    pub token: String,
}

/// forwarder → service：注册结果。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RelayHelloAck {
    pub ok: bool,
    pub message: String,
}

/// client → forwarder：请求路由到某个 service。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RelayConnect {
    pub target_service_id: String,
    pub token: String,
}

/// forwarder → client：路由结果。ok=true 后连接转为透传，client 继续跑 rdep 协议。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RelayConnectResp {
    pub ok: bool,
    /// 失败时对应 ErrorCode（如 5001 未找到目标 service / 1001 密钥错误）。
    pub code: u16,
    pub message: String,
}
