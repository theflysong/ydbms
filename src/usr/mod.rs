//! 用户系统:角色,口令认证,成员关系,以及"谁是超级用户".
//!
//! 这一层只做两件事:
//!
//! * **角色与口令**--[`User`] / [`UserStore`],存成服务目录下的 `users.json`;
//!   成员关系(谁能借用谁的权限)是 [`roles`] 里的几个纯函数;
//! * **服务目录与库**--[`service::ServiceDir`] / [`service::Catalog`] /
//!   [`service::Database`],见 [`service`] 的文件头.
//!
//! # 本版的口径(与 PostgreSQL 的差别,如实写在这里)
//!
//! * **用户与角色是同一个东西**(就是 [`User`]):`CREATE USER` 建的默认 `LOGIN`,
//!   `CREATE ROLE` / `CREATE GROUP` 建的默认 `NOLOGIN`;`SUPERUSER` / `CREATEDB` /
//!   `CREATEROLE` / `INHERIT` 都是属性.
//! * 没有连接协议,没有网络:`ysql` 是**单机**命令行,一个进程打开一个库.
//! * 口令存的是**加盐单轮 SHA-256**(见 [`hash`] 的文件头),不是 KDF.
//! * **超级用户不是继承来的**:只有当前角色自己 `is_admin` 才算(与 PostgreSQL
//!   一致);普通权限才沿成员关系继承(见 [`roles::effective_roles`]).
//! * 没有 `ADMIN OPTION` 那一档:角色只能由超级用户或 `CREATEROLE` 持有者授出.
//!
//! # `users.json`
//!
//! ```text
//! <server-dist>/users.json
//! { "version": 1,
//!   "users": [ { "name": "bob", "salt": "...32 位十六进制...",
//!                "password_hash": "...64 位十六进制...",
//!                "is_admin": true, "created_unix": 1760000000,
//!                "can_login": true, "can_create_db": true, "can_create_role": true,
//!                "inherit": true, "member_of": ["readers"] } ] }
//! ```
//!
//! **这是磁盘契约**:`version` 必存,新增字段一律 `#[serde(default)]`
//! (老文件要一直读得回来),改名或改结构就是改文件格式.后五个字段就是后加的,
//! 所以野外的老文件里没有它们--默认值必须让老文件**行为不变**(能登录,能继承).
//! `salt` / `password_hash` **可以为空** = 这个角色没有口令(登录时会被明确拒绝).

pub mod hash;
pub mod roles;
pub mod service;

pub use hash::{hash_password, random_salt, to_hex, verify, SALT_LEN};
pub use roles::{can_set_role, effective_roles, members_of, would_cycle};
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

/// 一个角色(用户也是角色,只差默认属性).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// 角色名(服务目录内唯一).
    pub name: String,
    /// 盐:`SALT_LEN` 字节的十六进制(见 [`hash`]);**空**表示没有口令.
    pub salt: String,
    /// `sha256(salt ‖ password)` 的十六进制;**空**表示没有口令.
    pub password_hash: String,
    /// 是不是超级用户:能连**任何**库,绕过一切权限检查(不是继承来的).
    #[serde(default)]
    pub is_admin: bool,
    /// 建立时间(unix 秒,见 [`now_unix`]).
    pub created_unix: u64,
    /// 能不能用口令登录(`LOGIN` / `NOLOGIN`).
    ///
    /// **默认 true**:野外的老 `users.json` 里的人都登录得进来,行为不变.
    #[serde(default = "yes")]
    pub can_login: bool,
    /// 能不能 `CREATE DATABASE`(`CREATEDB`).
    #[serde(default)]
    pub can_create_db: bool,
    /// 能不能建/改/删角色与授权(`CREATEROLE`).
    #[serde(default)]
    pub can_create_role: bool,
    /// 成员关系带来的权限会不会**自动**生效(`INHERIT` / `NOINHERIT`).
    ///
    /// `SET ROLE` 不受它影响(那是显式借身份),默认 true.
    #[serde(default = "yes")]
    pub inherit: bool,
    /// 这个角色**直接**属于哪些角色(`GRANT <角色> TO 他`).
    #[serde(default)]
    pub member_of: Vec<String>,
}

/// `serde` 默认值:`true`.
fn yes() -> bool {
    true
}

impl User {
    /// 有没有口令.没有口令的角色**登录不进来**(`AuthError::NoPassword`).
    pub fn has_password(&self) -> bool {
        !self.salt.is_empty() && !self.password_hash.is_empty()
    }

    /// 这个角色的属性.
    pub fn attrs(&self) -> RoleAttrs {
        RoleAttrs {
            admin: self.is_admin,
            login: self.can_login,
            create_db: self.can_create_db,
            create_role: self.can_create_role,
            inherit: self.inherit,
        }
    }
}

/// 一个角色的属性(`CREATE ROLE` 的那些选项).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleAttrs {
    /// `SUPERUSER`:能连任何库,绕过一切权限检查.
    pub admin: bool,
    /// `LOGIN`:能不能用 `ysql -U` 登录.
    pub login: bool,
    /// `CREATEDB`:能不能 `CREATE DATABASE`.
    pub create_db: bool,
    /// `CREATEROLE`:能不能管角色与授权.
    pub create_role: bool,
    /// `INHERIT`:成员关系带来的权限会不会自动生效.
    pub inherit: bool,
}

impl RoleAttrs {
    /// `CREATE USER` 的默认值:能登录.
    pub fn user() -> Self {
        RoleAttrs {
            admin: false,
            login: true,
            create_db: false,
            create_role: false,
            inherit: true,
        }
    }

    /// `CREATE ROLE` / `CREATE GROUP` 的默认值:**不能**登录.
    pub fn role() -> Self {
        RoleAttrs {
            login: false,
            ..RoleAttrs::user()
        }
    }

    /// bootstrap 的第一个用户:超级用户 + 全套开关(照 initdb 的口径,一眼看得懂).
    pub fn bootstrap() -> Self {
        RoleAttrs {
            admin: true,
            login: true,
            create_db: true,
            create_role: true,
            inherit: true,
        }
    }
}

/// 明文口令:只在内存里过一手.
///
/// **`Debug` 打不出来**(打印成 `Password(***)`):`Stmt` 之类派生着 `Debug`,
/// 哪天有人 `{:?}` 一个语句,明文口令就进了日志--这一条不许破.
/// 回显那条路(`Script::run_one_with`)也单独把它换成 `***`.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// 包一个明文口令.
    pub fn new(text: impl Into<String>) -> Self {
        Password(text.into())
    }

    /// 明文(只在真正要用它的地方取一次).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 空口令(作废的输入).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Password(***)")
    }
}

/// `ALTER ROLE ... PASSWORD ...` 要做的那一下.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordAction {
    /// 没提口令,不动它.
    Unchanged,
    /// `PASSWORD NULL`:清掉(这个角色以后登录不进来).
    Clear,
    /// `PASSWORD '...'`:换一个新的.
    Set(Password),
}

/// `ALTER ROLE` 里改的**一条**属性.
///
/// 用"改哪几条"而不是"整份属性",是因为 `ALTER ROLE r LOGIN` 不该把没提到的
/// `CREATEDB` 顺手抹掉(与 PostgreSQL 一致:只改写出来的那几条).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleAttrChange {
    /// `SUPERUSER` / `NOSUPERUSER`.
    Admin(bool),
    /// `LOGIN` / `NOLOGIN`.
    Login(bool),
    /// `CREATEDB` / `NOCREATEDB`.
    CreateDb(bool),
    /// `CREATEROLE` / `NOCREATEROLE`.
    CreateRole(bool),
    /// `INHERIT` / `NOINHERIT`.
    Inherit(bool),
}

impl RoleAttrChange {
    /// 把这一条盖到属性上.
    pub fn apply(&self, attrs: &mut RoleAttrs) {
        match self {
            RoleAttrChange::Admin(value) => attrs.admin = *value,
            RoleAttrChange::Login(value) => attrs.login = *value,
            RoleAttrChange::CreateDb(value) => attrs.create_db = *value,
            RoleAttrChange::CreateRole(value) => attrs.create_role = *value,
            RoleAttrChange::Inherit(value) => attrs.inherit = *value,
        }
    }

    /// 给用户看的那个词(状态行里用).
    pub fn keyword(&self) -> &'static str {
        match self {
            RoleAttrChange::Admin(value) => yes_no(*value, "SUPERUSER", "NOSUPERUSER"),
            RoleAttrChange::Login(value) => yes_no(*value, "LOGIN", "NOLOGIN"),
            RoleAttrChange::CreateDb(value) => yes_no(*value, "CREATEDB", "NOCREATEDB"),
            RoleAttrChange::CreateRole(value) => yes_no(*value, "CREATEROLE", "NOCREATEROLE"),
            RoleAttrChange::Inherit(value) => yes_no(*value, "INHERIT", "NOINHERIT"),
        }
    }
}

/// 二选一的那个词(属性开关与 `\du` 共用).
fn yes_no(value: bool, yes: &'static str, no: &'static str) -> &'static str {
    if value {
        yes
    } else {
        no
    }
}

/// `users.json` 的内容.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStore {
    /// 格式版本,见 [`USERS_VERSION`].
    pub version: u32,
    /// 角色表,按建立顺序.
    #[serde(default)]
    pub users: Vec<User>,
}

/// 认证失败的原因.
///
/// 与 [`DbError`] 分开:这里的几条都是**给用户看的**,`Display` 直接是中文,
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
    /// 角色在,但它不许登录(`NOLOGIN`).
    CannotLogin {
        /// 角色名.
        name: String,
    },
    /// 角色在,但它没有口令(没设过,或者被 `PASSWORD NULL` 清掉了).
    NoPassword {
        /// 角色名.
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
            AuthError::CannotLogin { name } => write!(f, "{}", tr!("usr.mod.011", arg0 = 
                name
            )),
            AuthError::NoPassword { name } => write!(f, "{}", tr!("usr.mod.012", arg0 = 
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

    /// 按名字找一个角色.
    pub fn find(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|user| user.name == name)
    }

    /// 按名字找一个角色(可变).
    pub fn find_mut(&mut self, name: &str) -> Option<&mut User> {
        self.users.iter_mut().find(|user| user.name == name)
    }

    /// 加一个角色:取新盐,算哈希,登记.
    ///
    /// `password` 是 `None`(不设口令,这个角色登录不进来)或非空口令
    /// (空串**拒绝**--那是打字打漏了,不是"不设口令",要后者就写 `None`).
    /// 重名与空名字都**拒绝**([`DbError::Denied`]).
    pub fn add(&mut self, name: &str, password: Option<&str>, attrs: RoleAttrs) -> Result<&User> {
        if name.trim().is_empty() {
            return Err(DbError::Denied {
                message: tr!("usr.mod.007").into_owned(),
            });
        }
        if password.is_some_and(|password| password.is_empty()) {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.008", arg0 =  name)),
            });
        }
        if self.find(name).is_some() {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.009", arg0 =  name)),
            });
        }

        let (salt, password_hash) = match password {
            Some(password) => {
                let salt = random_salt()?;
                (to_hex(&salt), hash_password(&salt, password))
            }
            // 没有口令:两个字段都空(`User::has_password` 就是看这个)
            None => (String::new(), String::new()),
        };

        self.users.push(User {
            name: name.to_string(),
            salt,
            password_hash,
            is_admin: attrs.admin,
            created_unix: now_unix(),
            can_login: attrs.login,
            can_create_db: attrs.create_db,
            can_create_role: attrs.create_role,
            inherit: attrs.inherit,
            member_of: Vec::new(),
        });

        // push 之后一定取得到
        Ok(self.users.last().expect(&tr!("usr.mod.010").into_owned()))
    }

    /// 换口令(`ALTER ROLE ... PASSWORD '...'`);`None` = 清掉(`PASSWORD NULL`).
    ///
    /// 每次都**取新盐**:同一个口令换两次得到的哈希也不一样(盐的用处).
    pub fn set_password(&mut self, name: &str, password: Option<&str>) -> Result<()> {
        if password.is_some_and(|password| password.is_empty()) {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.008", arg0 =  name)),
            });
        }

        let (salt, password_hash) = match password {
            Some(password) => {
                let salt = random_salt()?;
                (to_hex(&salt), hash_password(&salt, password))
            }
            None => (String::new(), String::new()),
        };

        let user = self.find_mut(name).ok_or_else(|| unknown_role(name))?;
        user.salt = salt;
        user.password_hash = password_hash;
        Ok(())
    }

    /// 改属性(`ALTER ROLE r SUPERUSER / LOGIN / ...`).
    pub fn set_attrs(&mut self, name: &str, attrs: RoleAttrs) -> Result<()> {
        let user = self.find_mut(name).ok_or_else(|| unknown_role(name))?;
        user.is_admin = attrs.admin;
        user.can_login = attrs.login;
        user.can_create_db = attrs.create_db;
        user.can_create_role = attrs.create_role;
        user.inherit = attrs.inherit;
        Ok(())
    }

    /// 改名:**连同别人 `member_of` 里的引用一起改**.
    ///
    /// 重名拒绝;名字换成自己等于什么都没做(照 PostgreSQL 不算错).
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if to.trim().is_empty() {
            return Err(DbError::Denied {
                message: tr!("usr.mod.007").into_owned(),
            });
        }
        if from == to {
            return Ok(());
        }
        if self.find(to).is_some() {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.013", arg0 =  to)),
            });
        }
        self.find_mut(from).ok_or_else(|| unknown_role(from))?.name = to.to_string();

        // 成员关系里存的是名字,得跟着改
        for user in &mut self.users {
            for role in &mut user.member_of {
                if role == from {
                    *role = to.to_string();
                }
            }
        }
        Ok(())
    }

    /// 删掉一个角色,**连同它出现在别人的 `member_of` 里的那些引用**.
    ///
    /// 调用方负责先判"能不能删"(最后一个超级用户 / 有库 / 有授权),见
    /// [`crate::usr::service::ServiceDir::drop_roles`].
    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.find(name).is_none() {
            return Err(unknown_role(name));
        }
        self.users.retain(|user| user.name != name);
        for user in &mut self.users {
            user.member_of.retain(|role| role != name);
        }
        Ok(())
    }

    /// `GRANT <角色> TO <成员>`:`member` 成为 `role` 的成员.
    ///
    /// 两个角色都得在;成环拒绝([`roles::would_cycle`]);已经是成员不算错.
    pub fn add_membership(&mut self, role: &str, member: &str) -> Result<()> {
        if role == member || roles::would_cycle(self, role, member) {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.mod.014", arg0 =  role, arg1 =  member)),
            });
        }
        if self.find(role).is_none() {
            return Err(unknown_role(role));
        }
        let user = self.find_mut(member).ok_or_else(|| unknown_role(member))?;
        if !user.member_of.iter().any(|held| held == role) {
            user.member_of.push(role.to_string());
        }
        Ok(())
    }

    /// `REVOKE <角色> FROM <成员>`:不再是成员;本来就不是也不算错.
    pub fn remove_membership(&mut self, role: &str, member: &str) -> Result<()> {
        let user = self.find_mut(member).ok_or_else(|| unknown_role(member))?;
        let before = user.member_of.len();
        user.member_of.retain(|held| held != role);
        if before == user.member_of.len() && self.find(role).is_none() {
            return Err(unknown_role(role));
        }
        Ok(())
    }

    /// 超级用户的个数.
    pub fn superusers(&self) -> usize {
        self.users.iter().filter(|user| user.is_admin).count()
    }

    /// 拿掉它之后还剩不剩超级用户?(`DROP ROLE` 与 `NOSUPERUSER` 都靠这一条)
    pub fn is_last_superuser(&self, name: &str) -> bool {
        match self.find(name) {
            Some(user) if user.is_admin => self.superusers() == 1,
            _ => false,
        }
    }

    /// 认证:用户必须在,必须允许登录,必须有口令且口令要对.
    pub fn authenticate(&self, name: &str, password: &str) -> std::result::Result<&User, AuthError> {
        let user = self
            .find(name)
            .ok_or_else(|| AuthError::UnknownUser {
                name: name.to_string(),
            })?;

        if !user.can_login {
            return Err(AuthError::CannotLogin {
                name: name.to_string(),
            });
        }
        if !user.has_password() {
            return Err(AuthError::NoPassword {
                name: name.to_string(),
            });
        }

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

/// "没有这个角色"的那一条(几处共用,免得到处拼同一句话).
pub(crate) fn unknown_role(name: &str) -> DbError {
    DbError::Denied {
        message: format!("{}", tr!("usr.mod.015", arg0 =  name)),
    }
}

fn io_err(path: &Path, e: std::io::Error) -> DbError {
    DbError::Io {
        path: path.display().to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}
