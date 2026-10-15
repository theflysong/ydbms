//! 用户系统:用户表,口令认证,以及"谁是管理员".
//!
//! 这一层只做两件事:
//!
//! * **用户与口令**--[`User`] / [`UserStore`],存成服务目录下的 `users.json`;
//! * **服务目录与库**--[`service::ServiceDir`] / [`service::Catalog`] /
//!   [`service::Database`],见 [`service`] 的文件头.
//!
//! # 本版的口径(与 PostgreSQL 的差别,如实写在这里)
//!
//! * **只做认证 + "owner(或管理员)才能连"**.`GRANT` / `REVOKE`,多用户共享,
//!   `CREATE USER` 都**明确不做**:用户要么由首次启动的 bootstrap(见
//!   [`UserStore::add`] 的调用方)建出来,要么手工编辑 `users.json`.
//! * 没有连接协议,没有网络:`ysql` 是**单机**命令行,一个进程打开一个库.
//! * 口令存的是**加盐单轮 SHA-256**(见 [`hash`] 的文件头),不是 KDF.
//!
//! # `users.json`
//!
//! ```text
//! <server-dist>/users.json
//! { "version": 1,
//!   "users": [ { "name": "bob", "salt": "...32 位十六进制...",
//!                "password_hash": "...64 位十六进制...",
//!                "is_admin": true, "created_unix": 1760000000 } ] }
//! ```
//!
//! **这是磁盘契约**:`version` 必存,新增字段一律 `#[serde(default)]`
//! (老文件要一直读得回来),改名或改结构就是改文件格式.

pub mod hash;
pub mod service;

pub use hash::{hash_password, random_salt, to_hex, verify, SALT_LEN};
pub use service::{Catalog, Database, ServiceDir};

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::db::table::{DbError, Result};

/// `users.json` 的格式版本.
pub const USERS_VERSION: u32 = 1;

/// 现在(unix 秒).
///
/// 不引 chrono:本版只需要"谁先谁后"和一个人能读的时间戳,秒就够了.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一个用户.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// 用户名(服务目录内唯一).
    pub name: String,
    /// 盐:`SALT_LEN` 字节的十六进制(见 [`hash`]).
    pub salt: String,
    /// `sha256(salt ‖ password)` 的十六进制.
    pub password_hash: String,
    /// 是不是管理员:管理员能连**任何**库(owner 检查对它放行).
    #[serde(default)]
    pub is_admin: bool,
    /// 建立时间(unix 秒,见 [`now_unix`]).
    pub created_unix: u64,
}

/// `users.json` 的内容.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStore {
    /// 格式版本,见 [`USERS_VERSION`].
    pub version: u32,
    /// 用户表,按建立顺序.
    #[serde(default)]
    pub users: Vec<User>,
}

/// 认证失败的原因.
///
/// 与 [`DbError`] 分开:这里的三条都是**给用户看的**,`Display` 直接是中文,
/// `ysql` 把它原样打出来(别的错误类型沿用了英文框架).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 服务目录里没有这个用户.
    UnknownUser {
        /// 被请求的用户名.
        name: String,
    },
    /// 用户在,但口令不对.
    BadPassword {
        /// 用户名.
        name: String,
    },
    /// `users.json` 被改坏了(盐不是合法十六进制).
    Corrupt {
        /// 用户名.
        name: String,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::UnknownUser { name } => write!(f, "{}", tr!("usr.mod.001", arg0 = 
                name
            )),
            AuthError::BadPassword { name } => write!(f, "{}", tr!("usr.mod.002", arg0 =  name)),
            AuthError::Corrupt { name } => write!(f, "{}", tr!("usr.mod.003", arg0 = 
                name
            )),
        }
    }
}

impl Default for UserStore {
    fn default() -> Self {
        UserStore::empty()
    }
}

impl UserStore {
    /// 一个空用户表(`version` 是当前版本).
    pub fn empty() -> Self {
        UserStore {
            version: USERS_VERSION,
            users: Vec::new(),
        }
    }

    /// `users.json` 在不在--**bootstrap 就靠它判断"是不是首次启动"**.
    pub fn exists(path: &Path) -> bool {
        path.is_file()
    }

    /// 读 `users.json`.
    ///
    /// 文件不存在,版本不认识,JSON 坏了都**报错**--不存在的判断请先用
    /// [`UserStore::exists`] ("不存在"与"空用户表"是两件不同的事).
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| io_err(path, e))?;
        let store: UserStore = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.mod.004", arg0 =  e)),
        })?;
        if store.version != USERS_VERSION {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("usr.mod.005", arg0 = 
                    store.version, arg1 =  USERS_VERSION
                )),
            });
        }
        Ok(store)
    }

    /// 写 `users.json`(临时文件 + rename,读者不会看到写了一半的用户表).
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.mod.006", arg0 =  e)),
        })?;

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{}\n", json)).map_err(|e| io_err(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| io_err(path, e))
    }

    /// 按名字找一个用户.
    pub fn find(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|user| user.name == name)
    }

    /// 加一个用户:取新盐,算哈希,登记.
    ///
    /// 重名与空口令都**拒绝**([`DbError::Denied`])--本版没有 `ALTER USER`,
    /// 悄悄改掉别人的口令比报错危险得多.
    pub fn add(&mut self, name: &str, password: &str, is_admin: bool) -> Result<&User> {
        if name.trim().is_empty() {
            return Err(DbError::Denied {
                message: tr!("usr.mod.007").into_owned(),
            });
        }
        if password.is_empty() {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.008", arg0 =  name)),
            });
        }
        if self.find(name).is_some() {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.009", arg0 =  name)),
            });
        }

        let salt = random_salt()?;
        self.users.push(User {
            name: name.to_string(),
            salt: to_hex(&salt),
            password_hash: hash_password(&salt, password),
            is_admin,
            created_unix: now_unix(),
        });

        // push 之后一定取得到
        Ok(self.users.last().expect(&tr!("usr.mod.010").into_owned()))
    }

    /// 认证:用户必须在,口令必须对.
    pub fn authenticate(&self, name: &str, password: &str) -> std::result::Result<&User, AuthError> {
        let user = self
            .find(name)
            .ok_or_else(|| AuthError::UnknownUser {
                name: name.to_string(),
            })?;

        let salt = hash::from_hex(&user.salt).ok_or_else(|| AuthError::Corrupt {
            name: user.name.clone(),
        })?;

        if verify(&salt, password, &user.password_hash) {
            Ok(user)
        } else {
            Err(AuthError::BadPassword {
                name: name.to_string(),
            })
        }
    }
}

fn io_err(path: &Path, e: std::io::Error) -> DbError {
    DbError::Io {
        path: path.display().to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}
