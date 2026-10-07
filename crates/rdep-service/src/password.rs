//! 口令散列与校验。
//!
//! ## 为什么不用裸 SHA-256
//!
//! 早期实现直接存 `sha256(password)`：既**无盐**（相同口令产生相同哈希，
//! 可被彩虹表一次性命中），又**无工作因子**（SHA-256 极快，GPU 每秒可试
//! 数十亿次）。对存放生产部署凭据的表来说这不可接受。
//!
//! 现采用 **PBKDF2-HMAC-SHA256**（随机 16 字节盐 + 可调迭代次数），
//! 存储格式：
//!
//! ```text
//! pbkdf2-sha256$<rounds>$<salt-hex>$<dk-hex>
//! ```
//!
//! ## 向后兼容
//!
//! `verify` 同时接受**旧的裸 SHA-256 十六进制**。`needs_rehash` 用于识别旧格式，
//! 由调用方在认证成功后**透明升级**为 PBKDF2 —— 部署升级后用户下一次登录即生效，
//! 无需强制改密。

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 派生密钥长度（字节），与 SHA-256 输出等长。
const DK_LEN: usize = 32;
/// 盐长度（字节）。
const SALT_LEN: usize = 16;
/// 存储格式前缀。
const PREFIX: &str = "pbkdf2-sha256";

/// PBKDF2 迭代次数。
///
/// 默认 100_000：release 构建下约 50ms（登录无感），同时提供真实的工作因子。
/// 注意 PBKDF2 的耗时随构建模式差异极大（release 100k≈50ms，debug 可达 1.6s），
/// 因此 debug/测试环境偏慢是预期行为。
///
/// **生产环境建议上调**：OWASP 2023 对 PBKDF2-HMAC-SHA256 的建议是 600k+，
/// 可用 `RDEP_PBKDF2_ROUNDS=600000`（更高强度，登录耗时相应增加）。
fn rounds() -> u32 {
    std::env::var("RDEP_PBKDF2_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v >= 1_000)
        .unwrap_or(100_000)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

/// 生成随机盐（失败时退化为计数器派生，保证功能不中断但强度下降）。
fn random_salt() -> Vec<u8> {
    let mut salt = vec![0u8; SALT_LEN];
    if getrandom::getrandom(&mut salt).is_err() {
        // 极端情况：退化为时间+pid 派生（强度下降，但不会导致无法创建账户）
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut h = HmacSha256::new_from_slice(b"rdep-salt-fallback")
            .expect("hmac key");
        h.update(&t.to_le_bytes());
        h.update(&std::process::id().to_le_bytes());
        let d = h.finalize().into_bytes();
        salt.copy_from_slice(&d[..SALT_LEN]);
    }
    salt
}

/// 计算 PBKDF2 派生密钥。
fn derive(password: &str, salt: &[u8], rounds: u32) -> Vec<u8> {
    let mut out = vec![0u8; DK_LEN];
    // 失败（仅在参数非法时）时退化为全 0，verify 会因此不匹配，不会误放行。
    let _ = pbkdf2::pbkdf2::<HmacSha256>(password.as_bytes(), salt, rounds, &mut out);
    out
}

/// 生成口令散列（用于创建/更新账户）。
pub fn hash_password(password: &str) -> String {
    let salt = random_salt();
    let r = rounds();
    let dk = derive(password, &salt, r);
    format!("{PREFIX}${r}${}${}", hex(&salt), hex(&dk))
}

/// 旧格式：裸 SHA-256 十六进制。
fn legacy_sha256_hex(password: &str) -> String {
    use sha2::Digest;
    let mut h = Sha256::new();
    h.update(password.as_bytes());
    hex(&h.finalize())
}

/// 校验口令是否匹配已存储的散列（同时支持旧格式）。
pub fn verify(stored: &str, password: &str) -> bool {
    if let Some(rest) = stored.strip_prefix(&format!("{PREFIX}$")) {
        let mut it = rest.split('$');
        let (Some(r), Some(salt_hex), Some(dk_hex)) = (it.next(), it.next(), it.next()) else {
            return false;
        };
        let (Ok(r), Some(salt), Some(want)) = (r.parse::<u32>(), unhex(salt_hex), unhex(dk_hex))
        else {
            return false;
        };
        if r == 0 || salt.is_empty() || want.len() != DK_LEN {
            return false;
        }
        let got = derive(password, &salt, r);
        // 常量时间比较，避免计时侧信道
        let mut diff = 0u8;
        for (a, b) in got.iter().zip(want.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    } else {
        // 旧格式：同样做常量时间比较
        let want = legacy_sha256_hex(password).into_bytes();
        let got = stored.as_bytes().to_vec();
        if got.len() != want.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in got.iter().zip(want.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

/// 该散列是否需要升级为 PBKDF2 格式（登录成功后调用方应据此重写存储）。
pub fn needs_rehash(stored: &str) -> bool {
    !stored.starts_with(&format!("{PREFIX}$"))
}


/// 常量时间字节串比较：相等返回 true。
///
/// 长度不同也要走完整个循环（比较 `a.len() ^ b.len()` 的折叠值），
/// 避免「长度是否相同」本身成为时序信号。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().min(b.len());
    for i in 0..n {
        diff |= a[i] ^ b[i];
    }
    // 长度不同时额外折叠，使循环次数不随长度差异泄漏
    for i in n..a.len().max(b.len()) {
        diff |= if i < a.len() { a[i] } else { 0 } | if i < b.len() { b[i] } else { 0 };
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash_password("s3cret");
        assert!(h.starts_with("pbkdf2-sha256$"), "格式: {h}");
        assert!(verify(&h, "s3cret"));
        assert!(!verify(&h, "s3cret "), "错误口令必须失败");
        assert!(!verify(&h, ""));
        assert!(!verify(&h, "S3CRET"), "应区分大小写");
    }

    /// 相同口令两次散列必须不同（验证确实加盐）。
    #[test]
    fn salt_makes_hashes_unique() {
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a, b, "相同口令应因随机盐产生不同散列");
        assert!(verify(&a, "same") && verify(&b, "same"), "两者都应能验证通过");
    }

    /// 旧的无盐 SHA-256 格式仍可验证，以便平滑升级。
    #[test]
    fn legacy_sha256_still_verifies() {
        let legacy = legacy_sha256_hex("admin");
        assert_eq!(legacy.len(), 64, "旧格式是裸 sha256 hex");
        assert!(!legacy.starts_with(PREFIX));
        assert!(verify(&legacy, "admin"));
        assert!(!verify(&legacy, "nope"));
        assert!(needs_rehash(&legacy), "旧格式应提示需要升级");
        assert!(!needs_rehash(&hash_password("x")), "新格式无需升级");
    }

    /// 畸形存储值不得误放行。
    #[test]
    fn malformed_stored_values_rejected() {
        for bad in [
            "",
            "pbkdf2-sha256$",
            "pbkdf2-sha256$abc$00$00",
            "pbkdf2-sha256$1000$zz$00",
            "pbkdf2-sha256$1000$00$",
            "pbkdf2-sha256$0$0011$22",
            "pbkdf2-sha256$1000$0011",
            "notahash",
        ] {
            assert!(!verify(bad, "admin"), "畸形散列 {bad:?} 不得通过验证");
        }
    }

    /// 常量时间比较：相等/不等/长度不同都正确。
    #[test]
    fn ct_eq_correctness() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"), "长度不同必须为 false");
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }

    /// 迭代次数记录在散列里，改小轮数不应让已存的散列失效（自描述）。
    #[test]
    fn rounds_are_self_describing() {
        std::env::set_var("RDEP_PBKDF2_ROUNDS", "1000");
        let h = hash_password("pw");
        assert!(h.contains("$1000$"), "应记录轮数: {h}");
        assert!(verify(&h, "pw"));
        std::env::remove_var("RDEP_PBKDF2_ROUNDS");
    }
}
