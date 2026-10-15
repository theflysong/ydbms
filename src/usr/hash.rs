//! 口令的加盐哈希.
//!
//! 存进 `users.json` 的是 `sha256(salt ‖ password)` 的**小写十六进制**,盐是每用户
//! 16 字节,来自 `/dev/urandom`.**绝不存明文**:`users.json` 就躺在用户主目录里,
//! 明文比加盐哈希危险得多.
//!
//! # 这不是 KDF
//!
//! 加盐**单轮** SHA-256 只解决两件事:同一个口令在不同用户下哈希不同(盐),
//! 文件里看不出原始口令.它**没有**迭代拉伸,抗离线暴力枚举远不如 bcrypt / argon2.
//! 本版是本地实验数据库,不做网络登录,所以按"够用且诚实"办--要升级就换算法,
//! 落点是 `users.json` 的 `version`(按它做迁移).
//!
//! # 盐从哪来
//!
//! 本机 toolchain 的 `std::random` 还是 unstable,所以直接读 `/dev/urandom`:本项目的
//! 运行环境是 Linux / WSL(`tests/repl_pty.rs` 也是 `#![cfg(unix)]`).**读不到就报错**,
//! 不回退到时间戳那种"可预测的随机"--那等于没有盐.

use std::fs::File;
use std::io::Read;

use sha2::{Digest, Sha256};

use crate::db::table::{DbError, Result};

/// 盐的字节数.
pub const SALT_LEN: usize = 16;

/// 系统随机源.
const URANDOM: &str = "/dev/urandom";

/// 取一份新盐([`SALT_LEN`] 字节,来自 `/dev/urandom`).
///
/// 读不到随机源就报 [`DbError::Io`],**不退化**成弱随机.
pub fn random_salt() -> Result<[u8; SALT_LEN]> {
    let mut salt = [0u8; SALT_LEN];
    let mut device = File::open(URANDOM).map_err(|e| io_err(e))?;
    device.read_exact(&mut salt).map_err(io_err)?;
    Ok(salt)
}

/// `sha256(salt ‖ password)` 的小写十六进制.
pub fn hash_password(salt: &[u8], password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(password.as_bytes());
    to_hex(&hasher.finalize())
}

/// 校验口令:按同样的盐算一遍,与 `expected_hex` 做**常量时间**比较.
///
/// `expected_hex` 里允许有前后空白(读文件时带进来的换行不影响结果).
pub fn verify(salt: &[u8], password: &str, expected_hex: &str) -> bool {
    let expected = expected_hex.trim();
    constant_time_eq(hash_password(salt, password).as_bytes(), expected.as_bytes())
}

/// 字节 → 小写十六进制.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 小写/大写十六进制 → 字节;遇到非十六进制字符或奇数长度返回 `None`.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = digit(pair[0])?;
        let lo = digit(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

/// 一个十六进制数字的值.
fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 不定长比较:长度不同直接不等,否则**每个字节都看一遍**(不提前返回).
///
/// 比的是哈希的十六进制串,长度是公开的,不是秘密,所以先比长度是安全的;
/// 真正要挡住的是"按前缀逐字节早退"给出的计时差异.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn io_err(e: std::io::Error) -> DbError {
    DbError::Io {
        path: URANDOM.to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}
