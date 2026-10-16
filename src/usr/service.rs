//! 服务目录:用户表与所有库放在哪儿,一个库目录长什么样,谁能连.
//!
//! ```text
//! <server-dist>/                     默认 ~/.ydbms(--server-dist 可换)
//! ├── users.json                    角色表(见 [`crate::usr`])
//! ├── .users.lock                   改 roles.json 时的 flock(读-改-写期间持有)
//! └── <库>/
//!     ├── catalog.json              {version, name, owner, created_unix, settings}
//!     ├── .lock                     单写者锁(flock;进程退出即释放)
//!     ├── manifest.json             提交记录:每张表已提交到哪儿(见 [`Manifest`])
//!     ├── <表>.heap                 基表:页式堆文件(表名带 schema 前缀时就是文件名)
//!     ├── <表>.schema.json          模式(格式归 [`crate::pers`])
//!     └── <表>.log                  行位置日志:挪过窝的行的新地址
//! ```
//!
//! **一个目录 = 一个库**,这与 [`crate::pers`] 的既有约定一致,也方便手工检查.
//! 表文件的读写规则**只在 [`crate::pers`] 一处**:这里只负责"库有哪些表,
//! 谁建的,现在能不能打开",一次 `TablePath::new(dir, 表名)` 就把两者接上.
//!
//! `<server-dist>` 是**相对路径**也一样:`ServiceDir::resolve` 在定下服务目录的那一刻
//! 就把它按当时的工作目录补成绝对路径(理由见那儿)--会话里的 `\cd` 改的是工作目录,
//! 改不动已经定下的服务目录.
//!
//! # 谁能连:owner(含继承来的身份)或超级用户
//!
//! `catalog.json` 的 `owner` 是一个**角色名**;判定看的是
//! [`roles::effective_roles`](crate::usr::roles::effective_roles)--当前角色自己,
//! 加上(按 `INHERIT` 逐边判断)它属于的那些角色.超级用户不在这个闭包里,它与
//! `owner` 是**并列**的两个放行条件.
//!
//! **权威判定在调用方**([`crate::cmd::engine::Store`]),因为"谁在连"是会话的事;
//! 这里只把"这个角色名能不能连这个库"这句口径落成一处([`ServiceDir::open`] 里那
//! 几行),别的角色的改,删,授权都交给 [角色操作](#角色操作)那一节.
//!
//! # 角色操作(`CREATE ROLE` / `ALTER ROLE` / `DROP ROLE` / `GRANT <角色>`)
//!
//! 这几条改的都是 `users.json`,而且是**读-改-写**:两个 `ysql` 进程(连着不同的库)
//! 同时建角色就会丢更新,所以一律先拿 `<server-dist>/.users.lock`
//! ([`ServiceDir::lock_users`],`try_lock`;拿不到就报错,不排队).
//!
//! 这一层只做**结构**上说得清的那几步:重名,找不到,成环,最后一个超级用户,
//! 改名前的"干净角色"检查.**"谁有资格做这件事"不在这里**(那要看会话的当前角色,
//! 由 `cmd` 的 `Store` 先判),与 `owner` 检查的既有分工一致.
//!
//! # `catalog.json` 是磁盘契约
//!
//! `owner` 就是 [`crate::usr::User::name`];`version` 不认识就报错.**缺文件不自动补**
//! --owner 无从知道,补一个等于伪造 owner,宁可不连.这也是"只有
//! `CREATE DATABASE` 会建出库目录"这句话的落点.`settings`(见 [`Settings`])是
//! 库级的会话默认值,**新增字段一律 `#[serde(default)]`**,老文件照旧读得回来.
//!
//! # 提交是原子的(4.2 / 7.5)
//!
//! 基表是**页式堆表**(数据在磁盘上,见 [`crate::pers::heap`]):写只往文件尾追加,
//! 而"写到哪儿算数"由 [`Manifest`] 说了算--**写清单那一刻才是提交点**.提交要写的
//! 只有两样:把挪过窝的行的新地址追加进 `<表>.log`,把每张表的位置记进清单;不自己
//! 落盘的表(兜底)才写**暂存**文件(`<表>.csv.new`)再推广.
//! **打开库时**([`Database::recover`])按记录收尾:堆文件截到记录的字节数,行位置
//! 日志截到记录的条数,还没推广完的补完,没有记录庇护的 `.new` 清掉--两次崩溃之间,
//! 库要么全在提交前,要么全在提交后.
//! 不做 WAL,也不 `fsync`:这条保证只覆盖**进程崩溃**(含 `kill -9`),不覆盖掉电.
//!
//! # 单写者锁
//!
//! 每个库目录上放一把 `flock`([`std::fs::File::try_lock`],Rust 1.89 起是标准库
//! 能力,**不用加依赖**).本版**不做多进程协调**:两个写者会互相踩,所以第二个
//! 进程直接**连不上**--宁可连不上,也不要写坏数据.锁是随句柄走的,进程退出
//! (哪怕被 `kill -9`)内核就释放,没有"陈旧锁文件"要清理.恢复也要在锁下做
//! ([`ServiceDir::open`] 里先上锁,再恢复).

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::rc::Rc;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::grants::Grants;
use super::roles;
use super::{
    now_unix, unknown_role, PasswordAction, RoleAttrChange, RoleAttrs, User, UserStore,
};
use crate::db::table::{is_file_stem_safe, DbError, Result, Table};
use crate::pers::csv::promote;
use crate::pers::heap::{self, truncate_moves, BufferPool, HeapFile, HeapTab};
use crate::pers::{CsvPersister, Persister, TablePath};

/// `catalog.json` 的格式版本.
pub const CATALOG_VERSION: u32 = 1;

/// 库元数据的文件名.
pub const CATALOG_FILE: &str = "catalog.json";

/// 单写者锁的文件名.
pub const LOCK_FILE: &str = ".lock";

/// 用户表的写锁文件名(`users.json` 是**全局**的,它自己那把锁).
pub const USERS_LOCK_FILE: &str = ".users.lock";

/// 提交记录的文件名(见 [`Manifest`]).
pub const MANIFEST_FILE: &str = "manifest.json";

/// `manifest.json` 的格式版本(TODO 7.7 把它升到 2:多了一张表的位置).
pub const MANIFEST_VERSION: u32 = 2;


/// 库级的会话默认值(`catalog.json` 的 `settings`).
///
/// 新连接**打开库时**把它套成会话的初始值--否则连回一个装载好的库,非限定表名
/// 一律解析不到,非得自己再 `SET search_path` 一次(`ALTER DATABASE ... SET` 就是
/// 为这件事加的).
///
/// **只影响以后的连接**:写它的那个会话自己的 `search_path` 不变,与 PostgreSQL
/// 一致(那条语句改的是"以后连上来的人").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// 库级默认 `search_path`;**空**表示不加前缀(即默认行为).
    #[serde(default)]
    pub search_path: Vec<String>,
}

/// 一个库的元数据(`<库>/catalog.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// 格式版本,见 [`CATALOG_VERSION`].
    pub version: u32,
    /// 库名(就是目录名).
    #[serde(default)]
    pub name: String,
    /// owner 的用户名--只有他(或管理员)能连这个库.
    pub owner: String,
    /// 建立时间(unix 秒).
    #[serde(default)]
    pub created_unix: u64,
    /// 库级的会话默认值,见 [`Settings`].
    ///
    /// **新增字段一律 `#[serde(default)]`**:老文件(没有这一项)照旧读得回来--
    /// 这不是可选项,野外已经有老 `catalog.json` 了.
    #[serde(default)]
    pub settings: Settings,
}

impl Catalog {
    /// 从**库目录**读元数据.
    pub fn load(dir: &Path) -> Result<Catalog> {
        let path = dir.join(CATALOG_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DbError::Format {
                    path: path.display().to_string(),
                    line: None,
                    message: tr!("usr.service.001").into_owned()
                        .to_string(),
                })
            }
            Err(e) => return Err(io_err(&path, e)),
        };

        let catalog: Catalog = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.service.002", arg0 =  e)),
        })?;

        if catalog.version != CATALOG_VERSION {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("usr.service.003", arg0 = 
                    catalog.version, arg1 =  CATALOG_VERSION
                )),
            });
        }
        Ok(catalog)
    }

    /// 写进**库目录**(临时文件 + rename).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CATALOG_FILE);
        let json = serde_json::to_string_pretty(self).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.service.004", arg0 =  e)),
        })?;

        let tmp = dir.join(format!("{}.tmp", CATALOG_FILE));
        fs::write(&tmp, format!("{}\n", json)).map_err(|e| io_err(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }
}

/// 一张表**已提交到磁盘**的那一部分(TODO 7.5).
///
/// 页式堆表的位置:`heap_bytes` 是堆文件里算数的字节数,`heap_rows` 是行目录里有
/// 多少行,`moved` 是行位置日志里算数的条数.三个数之外的东西都不算数--打开库时
/// 一律截掉,"要么全在提交前,要么全在提交后"就是这条.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableFootprint {
    /// 这张表是**页式堆表**吗(老格式的 CSV 基表在迁移之后就都是了).
    #[serde(default)]
    pub heap: bool,
    /// 堆文件里已提交的字节数.
    #[serde(default)]
    pub heap_bytes: u64,
    /// 行目录里已提交的行数.
    #[serde(default)]
    pub heap_rows: usize,
    /// 行位置日志里已提交的条数.
    #[serde(default)]
    pub moved: usize,
}

impl Default for TableFootprint {
    fn default() -> Self {
        TableFootprint {
            heap: false,
            heap_bytes: 0,
            heap_rows: 0,
            moved: 0,
        }
    }
}

/// 库目录里的**提交记录**(`manifest.json`,TODO 7.7).
///
/// 这份文件是那次提交的**commit record**:写进它之前,磁盘上的一切都还算"提交前"
/// (日志尾巴不算数,暂存文件不算数);写进它之后,那次提交就**必须做完**.
/// 于是打开库时只有两种可能,没有第三种:
///
/// * 清单里 `promoted` / `removed` 非空:推广暂存文件,删掉该删的--**幂等**,
///   崩在这一步的哪一处都能再跑一遍;
/// * 每张表的日志都截到清单里记的条数.
///
/// 与老版本那份一次性的 `commit.json` 的差别:这份**一直在**(提交完也留着),
/// 因为它记的是"磁盘上现在是哪一份",不只是"有一次提交没做完".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// 格式版本,见 [`MANIFEST_VERSION`].
    pub version: u32,
    /// 每张表提交之后的位置(生效名 → 行数).
    #[serde(default)]
    pub tables: BTreeMap<String, TableFootprint>,
    /// 这次提交要把哪些暂存文件推广进正式位置(新表,或者整理过的基表).
    #[serde(default)]
    pub promoted: Vec<String>,
    /// 这次提交要删掉文件的表(`DROP SCHEMA ... CASCADE` 的落点).
    #[serde(default)]
    pub removed: Vec<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            tables: BTreeMap::new(),
            promoted: Vec::new(),
            removed: Vec::new(),
        }
    }
}

impl Manifest {
    /// 读库目录里的提交记录;没有就是 `None`(老库,或者刚建的库).
    ///
    /// **读到坏的,或者版本不认识的,一律报错**:宁可开不了库,也不要静默地把
    /// "上次提交崩在中途"这件事当成没发生(那正是这份文件存在的理由).
    pub fn load(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(MANIFEST_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path, e)),
        };

        let manifest: Manifest = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.service.005", arg0 = e)),
        })?;
        if manifest.version != MANIFEST_VERSION {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!(
                    "{}",
                    tr!("usr.service.006", arg0 = manifest.version, arg1 = MANIFEST_VERSION)
                ),
            });
        }
        Ok(Some(manifest))
    }

    /// 读提交记录;没有就用一份空的(每张表的 `log` 都当 0).
    pub fn load_or_empty(dir: &Path) -> Result<Manifest> {
        Ok(Self::load(dir)?.unwrap_or_default())
    }

    /// 写进**库目录**(临时文件 + rename,与 [`Catalog::save`] 同一套).
    ///
    /// 这是**提交点**:这一行返回之后,这次提交就必须做完(见 [`Manifest`]).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(MANIFEST_FILE);
        let json = serde_json::to_string_pretty(self).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.service.007", arg0 = e)),
        })?;

        let tmp = dir.join(format!("{}.tmp", MANIFEST_FILE));
        fs::write(&tmp, format!("{}\n", json)).map_err(|e| io_err(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }

    /// 还有没有"没做完"的部分(推广 / 删表).
    pub fn settled(&self) -> bool {
        self.promoted.is_empty() && self.removed.is_empty()
    }
}

/// 服务目录:`users.json` 与各个库的父目录.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDir {
    root: PathBuf,
    /// 每个库的**缓冲池预算**(字节,见 [`crate::pers::heap`]).
    ///
    /// 由 `cmd` 按环境变量设一次(`ServiceDir::with_pool_bytes`),这里不读环境:
    /// 与 `root`/`home` 同一个口径--可测.
    pool_bytes: usize,
}

impl ServiceDir {
    /// 定下服务目录:给了 `root` 就用它,否则 `$HOME/.ydbms`.
    ///
    /// `home` 由调用方从环境里取(不在这儿读环境,于是可测).**没有 `HOME` 也不给
    /// `--server-dist` 就报错**--猜一个目录出来比报错糟糕得多.首次使用会自己把
    /// 目录树建出来.
    ///
    /// 相对路径**在这里当场按当时的工作目录定成绝对路径**:工作目录是会话里会被改
    /// 掉的东西(`\cd`,见 [`crate::cmd::engine`]),root 要是留着相对形式,`\cd` 之后
    /// 每一条 `join` 出来的路径(库目录,表文件,`catalog.json`)都跟着挪窝--库看起来
    /// "搬了家",表文件还会写到新目录下面去,与元数据分家.所以这个类型的不变量是
    /// **`root` 一定是绝对路径**([`ServiceDir::root`] 原样交给调用方显示).
    pub fn resolve(root: Option<PathBuf>, home: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(root) => root,
            None => match home {
                Some(home) => home.join(".ydbms"),
                None => {
                    return Err(DbError::Denied {
                        message: tr!("usr.service.008").into_owned(),
                    })
                }
            },
        };

        if root.as_os_str().is_empty() {
            return Err(DbError::Denied {
                message: tr!("usr.service.009").into_owned(),
            });
        }

        // 纯词法操作:不碰文件系统(此刻目录可能还不存在),也不追符号链接
        let root = std::path::absolute(&root).map_err(|e| io_err(&root, e))?;

        fs::create_dir_all(&root).map_err(|e| io_err(&root, e))?;
        Ok(ServiceDir {
            root,
            pool_bytes: crate::pers::heap::DEFAULT_POOL_BYTES,
        })
    }

    /// 设一次缓冲池预算(见 [`ServiceDir::pool_bytes`]).
    pub fn with_pool_bytes(mut self, bytes: usize) -> Self {
        self.pool_bytes = bytes;
        self
    }

    /// 服务目录的根.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `users.json` 的路径.
    pub fn users_path(&self) -> PathBuf {
        self.root.join("users.json")
    }

    /// 某个库的目录.
    pub fn database_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// 这个库在不在(目录在,且 `catalog.json` 在).
    pub fn exists(&self, name: &str) -> bool {
        self.root.join(name).join(CATALOG_FILE).is_file()
    }

    /// 服务目录下所有库的名字(按字典序).
    pub fn databases(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let entries = fs::read_dir(&self.root).map_err(|e| io_err(&self.root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.root, e))?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if entry.path().join(CATALOG_FILE).is_file() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// 读一个库的元数据.
    pub fn catalog(&self, name: &str) -> Result<Catalog> {
        let name = checked_name(name)?;
        Catalog::load(&self.database_dir(&name))
    }

    /// `CREATE DATABASE name`:建目录,写元数据.返回给用户看的一行状态.
    pub fn create_database(&self, name: &str, owner: &str) -> Result<String> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);

        if dir.join(CATALOG_FILE).is_file() {
            return Err(DbError::DuplicateDatabase { name });
        }

        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        Catalog {
            version: CATALOG_VERSION,
            name: name.clone(),
            owner: owner.to_string(),
            created_unix: now_unix(),
            settings: Settings::default(),
        }
        .save(&dir)?;

        Ok(format!("{}", tr!("usr.service.010", arg0 =  name, arg1 =  owner)))
    }

    /// `ALTER DATABASE name SET search_path TO ...`:把库级默认值记进元数据.
    ///
    /// 读-改-写:每次都以磁盘上的那一份为准,免得覆盖掉别人刚改的字段.owner 检查
    /// 不在这里--"谁在连"是会话的事,由调用方(`cmd::engine::Store`)先判.
    ///
    /// **别人正连着这个库也允许**(不像 `DROP DATABASE`):改的只是一个字段的
    /// 原子改写,不影响任何人手里的数据,而且照 PostgreSQL 的口径它只对**以后的
    /// 连接**生效.
    pub fn set_database_search_path(&self, name: &str, path: &[String]) -> Result<String> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);
        if !dir.join(CATALOG_FILE).is_file() {
            return Err(DbError::UnknownDatabase { name });
        }

        let mut catalog = Catalog::load(&dir)?;
        catalog.settings.search_path = path.to_vec();
        catalog.save(&dir)?;

        let shown = match path.is_empty() {
            true => tr!("usr.service.011").into_owned(),
            false => path.join(","),
        };
        Ok(format!("{}", tr!("usr.service.012", arg0 = 
            name, arg1 =  shown
        )))
    }

    /// `DROP DATABASE [IF EXISTS] name`:删掉整个库目录.返回一行状态.
    ///
    /// 别人正连着这个库就**不删**(先探一次单写者锁):删掉一个有人正在写的库,
    /// 比让他连不上糟糕得多.
    pub fn drop_database(&self, name: &str, if_exists: bool) -> Result<String> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);

        if !dir.join(CATALOG_FILE).is_file() {
            return if if_exists {
                Ok(format!("{}", tr!("usr.service.013", arg0 =  name)))
            } else {
                Err(DbError::UnknownDatabase { name })
            };
        }

        let probe = File::create(dir.join(LOCK_FILE)).map_err(|e| io_err(&dir, e))?;
        match probe.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DbError::Denied {
                    message: format!("{}", tr!("usr.service.014", arg0 =  name)),
                })
            }
            Err(TryLockError::Error(e)) => return Err(io_err(&dir, e)),
        }
        drop(probe);

        fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        Ok(format!("{}", tr!("usr.service.015", arg0 =  name)))
    }

    /// 打开一个库:owner 检查 → 上单写者锁 → 返回句柄.
    ///
    /// 顺序要紧:**先上锁再让调用方载入表**,否则会在载入的半途被另一个写者改文件.
    pub fn open(&self, name: &str, user: &User) -> Result<Database> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);

        if !dir.is_dir() {
            return Err(DbError::UnknownDatabase { name });
        }

        // 目录在,`catalog.json` 不在:说清是"这不是本版建的库",而不是含糊的"库不存在"
        let catalog = Catalog::load(&dir)?;
        // 授权表也在这时候读一次(缺文件 = 没有授权):`CONNECT` 是"能不能连"的
        // 第三个放行条件,把它装进 `Database` 之后,这个会话的表权限也照它判.
        let grants = Grants::load_or_empty(&dir)?;
        if !user.is_admin
            && !self.acts_as(&user.name, &catalog.owner)?
            && !grants.may_connect(&user.name)
        {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.service.016", arg0 = 
                    name, arg1 =  catalog.owner, arg2 =  user.name
                )),
            });
        }

        let lock_path = dir.join(LOCK_FILE);
        let lock = File::create(&lock_path).map_err(|e| io_err(&lock_path, e))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DbError::Denied {
                    message: format!("{}", tr!("usr.service.017", arg0 =  name)),
                })
            }
            Err(TryLockError::Error(e)) => return Err(io_err(&lock_path, e)),
        }

        let mut database = Database {
            dir,
            name,
            catalog,
            grants,
            manifest: Manifest::default(),
            pool: Rc::new(BufferPool::new(self.pool_bytes)),
            lock,
        };

        // 上次崩在提交中途:**拿到锁之后,谁来读表之前**把它补完(见
        // [`Database::recover`]).放在这里而不是 `load_tables` 里,是因为恢复要
        // 改文件--必须在锁下做.
        database.recover()?;
        Ok(database)
    }

    // ------------------------------------------------- 借别人的库的锁(6.4)

    /// 借 `name` 这个库的单写者锁,做一次**读-改-写**(跨库的 `GRANT CONNECT` 与
    /// `ALTER DATABASE ... OWNER TO` 走它).
    ///
    /// 三条规矩:
    ///
    /// * **拿不到锁就不改**:别人正连着这个库,它自己的会话手里才是最新那一份,
    ///   我们改就是丢更新--宁可报错(与"第二个写者连不上"同一个口径);
    /// * **本会话正开着的那个库不走这里**:同进程对同一个文件再 `try_lock` 必然
    ///   失败(flock 跟着句柄走),那种情况调用方直接用手里那个
    ///   [`Database::set_grants`];
    /// * **不跑恢复**:这里只借锁读改写目录里的两个小文件,不碰表文件--上次崩在
    ///   提交中途的那份现场留给"真正打开这个库"的那一次([`Database::recover`]).
    pub fn with_grants<T>(
        &self,
        name: &str,
        f: impl FnOnce(&mut Grants) -> Result<T>,
    ) -> Result<T> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);
        if !dir.join(CATALOG_FILE).is_file() {
            return Err(DbError::UnknownDatabase { name });
        }
        let _lock = self.lock_database(&name, &dir)?;

        let mut grants = Grants::load_or_empty(&dir)?;
        let outcome = f(&mut grants)?;
        grants.save(&dir)?;
        Ok(outcome)
    }

    /// `ALTER DATABASE name OWNER TO owner`:换库的 owner(借锁读改写 `catalog.json`).
    ///
    /// 库在不在,新 owner 存不存在,"谁有资格换"都由调用方先判
    /// (`cmd` 的 `Store`--那是会话的事).
    pub fn set_database_owner(&self, name: &str, owner: &str) -> Result<String> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);
        if !dir.join(CATALOG_FILE).is_file() {
            return Err(DbError::UnknownDatabase { name });
        }
        let _lock = self.lock_database(&name, &dir)?;

        let mut catalog = Catalog::load(&dir)?;
        let previous = std::mem::replace(&mut catalog.owner, owner.to_string());
        catalog.save(&dir)?;

        Ok(format!("{}", tr!("usr.service.036", arg0 = 
            name, arg1 =  previous, arg2 =  owner
        )))
    }

    /// 借一个库目录的锁;拿不到就报错(**不排队**).
    fn lock_database(&self, name: &str, dir: &Path) -> Result<File> {
        let path = dir.join(LOCK_FILE);
        let lock = File::create(&path).map_err(|e| io_err(&path, e))?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(TryLockError::WouldBlock) => Err(DbError::Denied {
                message: format!("{}", tr!("usr.service.037", arg0 =  name)),
            }),
            Err(TryLockError::Error(e)) => Err(io_err(&path, e)),
        }
    }

    // ------------------------------------------------------------ 角色(6.4)

    /// 读 `users.json`,**文件不在**时给一句说得清的拒绝.
    ///
    /// 首次启动的 bootstrap 在 `ysql` 那一条路上(它先建出 `users.json` 才开会话),
    /// 所以走到这里的"文件不在"只有两种:有人把它删了,或者会话是手工接线的.
    /// 两种都不能悄悄当成"空用户表"--那样下一次启动会以为早就 bootstrap 过,
    /// 谁都进不来.
    fn load_users(&self) -> Result<UserStore> {
        if !self.users_path().is_file() {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.service.035", arg0 = 
                    self.users_path().display()
                )),
            });
        }
        self.users()
    }

    /// `users.json` 的角色表(读一次,自己判"谁做什么").
    ///
    /// 认证只管"这个人是不是他",做不做得了某件事要看**当前角色**的属性与成员关系,
    /// 所以 `cmd` 的 `Store` 每回都读一份新的--文件很小,而"另一个进程刚改过"
    /// 比"缓存快一点"重要.
    pub fn users(&self) -> Result<UserStore> {
        UserStore::load(&self.users_path())
    }

    /// `role` 算不算 `owner`:owner 在它的**有效角色集**里.
    ///
    /// 有效角色集 = 自己 + 按 `INHERIT` 逐边继承来的那些身份
    /// ([`roles::effective_roles`]);超级用户**不在**这个集合里,它与 owner 是并列的
    /// 两个放行条件(调用方自己判超级用户).
    ///
    /// 没有 `users.json` 时只认他自己(手工/测试里手搓的角色,或者还没 bootstrap):
    /// 这不是放行--owner 判定照旧要名字对得上.
    pub fn acts_as(&self, role: &str, owner: &str) -> Result<bool> {
        if role == owner {
            return Ok(true);
        }
        if !self.users_path().is_file() {
            return Ok(false);
        }
        Ok(roles::effective_roles(&self.users()?, role).contains(owner))
    }

    /// `users.json` 之外还记着这个角色名的地方:库的 owner,以及库里的授权记录.
    ///
    /// 返回库名(按字典序,去重).`DROP ROLE` 与 `ALTER ROLE ... RENAME TO` 靠它
    /// 说清"卡在哪几个库":那些文件分散在各库目录里,而那些库可能正被别的进程锁着,
    /// 所以**改不干净的地方就不改**,报错让用户自己去清.
    ///
    /// 读不出来就报错(不猜):`catalog.json` 坏了的库恰恰是最不该随手绕过的时候.
    pub fn role_traces(&self, role: &str) -> Result<Vec<String>> {
        let mut traces = Vec::new();
        for database in self.databases()? {
            let dir = self.database_dir(&database);
            let owner = Catalog::load(&dir)?.owner == role;
            let granted = Grants::load(&dir)?.is_some_and(|grants| grants.mentions(role));
            if owner || granted {
                traces.push(database);
            }
        }
        Ok(traces)
    }

    /// 这个角色必须在(授权 / 换 owner 之前先问一句).
    ///
    /// 没有 `users.json` 时**拒绝**:那时没法校验"这个人存在不存在",而授权表里写
    /// 一个不存在的名字,只是在给以后留一个说不清的幽灵.
    pub fn require_user(&self, name: &str) -> Result<()> {
        let store = self.load_users()?;
        if store.find(name).is_none() {
            return Err(unknown_role(name));
        }
        Ok(())
    }

    /// 拿 `<server-dist>/.users.lock`(读-改-写期间持有).
    ///
    /// `try_lock`:**拿不到就报错,不排队**--另一个进程正在改用户表时等下去,
    /// 只会把两个进程都挂住;与库的单写者锁同一个口径("宁可连不上,也不要写坏数据").
    /// 锁随句柄走,返回值 drop 掉(函数返回)内核就放.
    fn lock_users(&self) -> Result<File> {
        let path = self.root.join(USERS_LOCK_FILE);
        let lock = File::create(&path).map_err(|e| io_err(&path, e))?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(TryLockError::WouldBlock) => Err(DbError::Denied {
                message: format!("{}", tr!("usr.service.021")),
            }),
            Err(TryLockError::Error(e)) => Err(io_err(&path, e)),
        }
    }

    /// `CREATE USER` / `CREATE ROLE` / `CREATE GROUP`:读-改-写 `users.json`.
    ///
    /// `add_to` 是 `IN ROLE a, b` 那几个(建完顺手加成员)."谁有资格建角色"在
    /// 调用方(`cmd` 的 `Store`)先判.
    pub fn create_role(
        &self,
        name: &str,
        password: Option<&str>,
        attrs: RoleAttrs,
        add_to: &[String],
    ) -> Result<String> {
        let _lock = self.lock_users()?;
        let path = self.users_path();
        let mut store = self.load_users()?;

        store.add(name, password, attrs)?;
        for role in add_to {
            if store.find(role).is_none() {
                return Err(unknown_role(role));
            }
            store.add_membership(role, name)?;
        }
        store.save(&path)?;

        let login = match attrs.login {
            true => tr!("usr.service.022").into_owned(),
            false => tr!("usr.service.023").into_owned(),
        };
        Ok(format!("{}", tr!("usr.service.024", arg0 =  name, arg1 =  login)))
    }

    /// `ALTER USER` / `ALTER ROLE`:改属性,换口令,改名.
    ///
    /// 改名要过**干净角色**那一关([`ServiceDir::role_traces`]),理由写在那里;
    /// 摘掉最后一个超级用户也拒绝(与 bootstrap 的"否则谁都进不去"同一个道理).
    /// 中途任何一条拒绝都发生在 `save` 之前,所以磁盘上一个字都没动.
    pub fn alter_role(
        &self,
        name: &str,
        new_name: Option<&str>,
        password: PasswordAction,
        changes: &[RoleAttrChange],
    ) -> Result<String> {
        let _lock = self.lock_users()?;
        let path = self.users_path();
        let mut store = self.load_users()?;

        let mut attrs = store
            .find(name)
            .ok_or_else(|| unknown_role(name))?
            .attrs();
        for change in changes {
            change.apply(&mut attrs);
        }
        if !attrs.admin && store.is_last_superuser(name) {
            return Err(DbError::Denied {
                message: format!("{}", tr!("usr.service.025", arg0 =  name)),
            });
        }

        store.set_attrs(name, attrs)?;
        match password {
            PasswordAction::Unchanged => {}
            PasswordAction::Clear => store.set_password(name, None)?,
            PasswordAction::Set(ref text) => store.set_password(name, Some(text.as_str()))?,
        }

        let renamed = match new_name {
            Some(new_name) if new_name != name => {
                let traces = self.role_traces(name)?;
                if !traces.is_empty() {
                    return Err(DbError::Denied {
                        message: format!("{}", tr!("usr.service.026", arg0 = 
                            name, arg1 =  traces.join(",")
                        )),
                    });
                }
                store.rename(name, new_name)?;
                true
            }
            _ => false,
        };

        store.save(&path)?;
        let shown = match (renamed, new_name) {
            (true, Some(new_name)) => format!("{}", tr!("usr.service.027", arg0 = 
                name, arg1 =  new_name
            )),
            _ => format!("{}", tr!("usr.service.028", arg0 =  name)),
        };
        Ok(shown)
    }

    /// `DROP USER` / `DROP ROLE`(`IF EXISTS` 命中只发一句提示).
    ///
    /// 不放过:最后一个超级用户,拥有库的角色,还有在某个库里留着授权的角色--后两条
    /// 都点名"卡在哪几个库"(理由见 [`ServiceDir::role_traces`])."别删自己"在
    /// 调用方判(那是会话的事).
    pub fn drop_roles(&self, names: &[String], if_exists: bool) -> Result<String> {
        let _lock = self.lock_users()?;
        let path = self.users_path();
        let mut store = self.load_users()?;

        let mut missing = Vec::new();
        for name in names {
            if store.find(name).is_none() {
                if if_exists {
                    missing.push(name.clone());
                    continue;
                }
                return Err(unknown_role(name));
            }
            if store.is_last_superuser(name) {
                return Err(DbError::Denied {
                    message: format!("{}", tr!("usr.service.029", arg0 =  name)),
                });
            }
            let traces = self.role_traces(name)?;
            if !traces.is_empty() {
                return Err(DbError::Denied {
                    message: format!("{}", tr!("usr.service.030", arg0 = 
                        name, arg1 =  traces.join(",")
                    )),
                });
            }
        }

        let mut dropped = Vec::new();
        for name in names {
            if store.find(name).is_some() {
                store.remove(name)?;
                dropped.push(name.clone());
            }
        }
        store.save(&path)?;

        let mut parts = Vec::new();
        if !dropped.is_empty() {
            parts.push(format!("{}", tr!("usr.service.031", arg0 =  dropped.join(","))));
        }
        if !missing.is_empty() {
            parts.push(format!("{}", tr!("usr.service.032", arg0 =  missing.join(","))));
        }
        Ok(parts.join(" "))
    }

    /// `GRANT <角色> TO <成员>` / `REVOKE <角色> FROM <成员>`.
    ///
    /// 成环由 [`UserStore::add_membership`] 拒绝;"谁有资格授角色"在调用方判.
    pub fn grant_roles(
        &self,
        roles: &[String],
        members: &[String],
        is_grant: bool,
    ) -> Result<String> {
        let _lock = self.lock_users()?;
        let path = self.users_path();
        let mut store = self.load_users()?;

        for role in roles {
            if store.find(role).is_none() {
                return Err(unknown_role(role));
            }
        }
        for member in members {
            if store.find(member).is_none() {
                return Err(unknown_role(member));
            }
            for role in roles {
                if is_grant {
                    store.add_membership(role, member)?;
                } else {
                    store.remove_membership(role, member)?;
                }
            }
        }
        store.save(&path)?;

        Ok(match is_grant {
            true => format!("{}", tr!("usr.service.033", arg0 = 
                roles.join(","), arg1 =  members.join(",")
            )),
            false => format!("{}", tr!("usr.service.034", arg0 = 
                roles.join(","), arg1 =  members.join(",")
            )),
        })
    }
}

/// 一个打开的库:目录,元数据,单写者锁.
///
/// 句柄活着的时候别人连不上同一个库;它也顺手管住表文件的读写(规则在
/// [`crate::pers`],这里只是把"库目录 + 表名"接起来).
#[derive(Debug)]
pub struct Database {
    dir: PathBuf,
    name: String,
    catalog: Catalog,
    /// 这个库的授权表(打开时读一次;缺文件 = 空的).
    ///
    /// **会话手里的这一份就是当下那一份**:它是唯一能改这个库的写者(单写者锁),
    /// 而跨库改别人库的授权要走 [`ServiceDir::with_grants`].
    grants: Grants,
    /// 这个库的**提交记录**(打开时读一次,每次提交之后更新).
    ///
    /// 与 `grants` 一样:会话手里这一份就是当下那一份(单写者锁保证了没人能同时改).
    manifest: Manifest,
    /// 这个库的**缓冲池**(所有页式基表共用一份,见 [`crate::pers::heap`]).
    ///
    /// 预算决定"最多有多少页常驻",于是 `ysql` 的常驻内存与**库的大小无关**.
    pool: Rc<BufferPool>,
    /// 单写者锁.**只是持有**:句柄关掉(含进程退出,被 `kill -9`)内核就释放.
    lock: File,
}

impl Database {
    /// 库名.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 库目录.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// owner 的用户名.
    pub fn owner(&self) -> &str {
        &self.catalog.owner
    }

    /// 库元数据.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// 这个库的授权表.
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// 换一份授权表并写回 `<库>/grants.json`(临时文件 + rename).
    pub fn set_grants(&mut self, mut grants: Grants) -> Result<()> {
        grants.save(&self.dir)?;
        self.grants = grants;
        Ok(())
    }

    /// 这些表已经没了:把它们名下的授权记录一并删掉(没动过就不写盘).
    pub fn retire_grants(&mut self, tables: &[String]) -> Result<()> {
        if !self.grants.prune_tables(tables) {
            return Ok(());
        }
        let grants = self.grants.clone();
        self.set_grants(grants)
    }

    /// 库里的**缓冲池**(给 `CREATE TABLE` 造页式堆表用的那一份).
    pub fn pool(&self) -> Rc<BufferPool> {
        Rc::clone(&self.pool)
    }

    /// 提交记录里每张表的位置(只读;`CREATE TABLE` 造页式堆表时要问它).
    pub fn manifest_tables(&self) -> &BTreeMap<String, TableFootprint> {
        &self.manifest.tables
    }

    /// 库目录里的堆文件路径(表名 → 文件名主干).
    pub fn heap_path(&self, name: &str) -> PathBuf {
        TablePath::new(&self.dir, name).heap_path()
    }

    /// 把库里所有表读进来.
    ///
    /// 扫 `*.heap`(TODO 7.5 之后的基表格式):每个都按模式文件的形状打开一份页式
    /// 堆表--**只扫记录头建目录**,行体一行都不读.老格式的 `*.csv` 在
    /// [`Database::recover`] 里已经搬过来了,这里见不到它们.
    ///
    /// 文件主干名与模式里的表名要一致:不一致意味着有人手工动过目录,继续用会写出
    /// 两份文件,留下幽灵表.
    pub fn load_tables(&self) -> Result<Vec<Box<dyn Table>>> {
        let persister = CsvPersister::new();
        let mut paths = Vec::new();

        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("heap") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut tables: Vec<Box<dyn Table>> = Vec::with_capacity(paths.len());
        for path in paths {
            let at = TablePath::new(&self.dir, &stem_of(&path));
            let (name, shape) = persister.load_schema(&at)?;
            let stem = stem_of(&path);
            if stem != name {
                return Err(DbError::Format {
                    path: path.display().to_string(),
                    line: None,
                    message: format!("{}", tr!("usr.service.018", arg0 = stem, arg1 = name)),
                });
            }

            let footprint = self
                .manifest
                .tables
                .get(&stem)
                .copied()
                .unwrap_or_default();
            let file = Rc::new(HeapFile::open(&path, footprint.heap_bytes, self.pool())?);
            let mut table = HeapTab::open(&name, shape, file, footprint.heap_rows)?;
            // 挪过窝的那些行:新地址在 `<表>.log` 里(见 `pers::heap`)
            table.apply_moved_log(&at.log_path(), footprint.moved)?;
            tables.push(Box::new(table));
        }
        Ok(tables)
    }

    /// 一次**原子**的多表提交:写 `tables`,删 `removed`,两者要么全做,要么全不做.
    ///
    /// 协议(TODO 7.5):**基表的写是追加式的**,所以提交点只做两件事--把挪过窝的
    /// 行的新地址追加进 `<表>.log`,把每张表的位置(堆文件字节数 / 行数 / 位置日志
    /// 条数)写进 `manifest.json`.写清单那一刻是**提交点**:它之前的东西都不算数
    /// (打开库时截掉),它之后的都必须做完(推广与删表,幂等).
    ///
    /// 不走这条路的表(不自己落盘的那种,兜底)仍然整表写成暂存文件再推广.
    ///
    /// 中途失败(IO 错,进程被杀)只会停在三种位置之一,而这三种在重启时都有确定的
    /// 去向:清单没写(尾部的字节不算数,截掉就是提交前);清单写了(推广与删表补完
    /// 就是提交后);清空那一遍没写成(再跑一遍,还是提交后).**没有"改了一半"
    /// 这种状态**.
    ///
    /// 表名**就是文件名主干**,所以这里是"名字能不能当文件名"的最后一道闸
    /// (见 [`crate::db::table::is_file_stem_safe`]):`ir` 建表时已经拒过一次,
    /// 但真正碰文件系统的是这里--**这道闸不能省**,名字里带 `../` 真的会写到
    /// 别人的库目录(甚至服务目录)里去.
    pub fn commit(&mut self, tables: &[&dyn Table], removed: &[String]) -> Result<()> {
        let persister = CsvPersister::new();
        let mut manifest = self.manifest.clone();
        let mut promoted = Vec::new();

        for table in tables {
            let name = checked_table_name(table.name())?;
            let at = TablePath::new(&self.dir, &name);
            // 模式文件每次提交都写一遍:它只有几百字节,而"表的结构"必须跟着表走
            // (数据由堆文件自己管,模式不走它)
            persister.save_schema(*table, &at)?;
            let Some(paged) = table.paged() else {
                // 兜底:不自己落盘的表整表重写(暂存 + 推广)
                persister.stage(*table, &at)?;
                manifest.tables.insert(name.clone(), TableFootprint::default());
                promoted.push(name);
                continue;
            };

            // 挪过窝的行:新地址追加进位置日志(账本里那一条数只有在清单写成功之后
            // 才算数)
            let before = self.manifest.tables.get(&name).copied().unwrap_or_default();
            let moves = paged.write_move_log(&at.log_path(), before.moved)?;

            let (bytes, rows) = paged.footprint();
            manifest.tables.insert(
                name,
                TableFootprint {
                    heap: true,
                    heap_bytes: bytes,
                    heap_rows: rows,
                    moved: moves,
                },
            );
        }
        for name in removed {
            manifest.tables.remove(name);
        }

        // 提交点
        manifest.promoted = promoted;
        manifest.removed = removed.to_vec();
        manifest.save(&self.dir)?;

        // 做到底,然后把现场"落定"
        self.apply(&manifest)?;
        manifest.promoted.clear();
        manifest.removed.clear();
        manifest.save(&self.dir)?;

        // 认账:从这以后写的东西才算下一轮.`mark_committed` 要改表自己的状态,
        // 而这里拿到的是 `&dyn Table`--所以走库目录里那一份**存储句柄**:
        // 页式基表的文件是 `Rc` 共享的,提交点只要把"已提交长度"往前挪一格.
        for table in tables {
            if let Some(paged) = table.paged() {
                paged.mark_committed();
            }
        }
        self.manifest = manifest;
        Ok(())
    }

    /// 把一个提交记录做到底:推广暂存文件,删掉要删的表.    /// 把一个提交记录做到底:推广暂存文件,删掉要删的表.
    ///
    /// **幂等**:已经搬过的暂存文件(rename 掉之后 `.new` 就没了)会被跳过,
    /// 删掉的文件再删一次也不算错--所以它可以被"崩在中途"之后的恢复直接重跑.
    fn apply(&self, manifest: &Manifest) -> Result<()> {
        for name in &manifest.promoted {
            let name = checked_table_name(name)?;
            let at = TablePath::new(&self.dir, &name);
            match manifest.tables.get(&name).is_some_and(|at| at.heap) {
                // 页式堆表暂存的是 `<表>.heap.new`
                true => heap::promote(&at.heap_path())?,
                false => promote(&at)?,
            }
        }
        for name in &manifest.removed {
            self.remove_table_files(name)?;
        }
        Ok(())
    }

    /// 打开库时把上次没写完的提交收尾,把老格式的表搬进堆文件,并清扫残渣.
    ///
    /// 四步,顺序不能换:
    ///
    /// 1. 清单里还有 `promoted` / `removed` 的,先做到底(新数据早就在暂存文件里了,
    ///    这是"全在提交后"),然后把那两栏清空重写;
    /// 2. **老格式(CSV 基表)整表搬进堆文件**(TODO 7.5 的迁移):写堆文件 -> 写清单
    ///    (提交点)-> 删掉 CSV.崩在中间时,清单里还没有这张表的位置,下次接着搬
    ///    (堆文件重写一遍,幂等);
    /// 3. 每张表截到清单里记的位置:行位置日志截到记的条数(堆文件由
    ///    [`HeapFile::open`] 在打开时截);
    /// 4. 清扫没有清单庇护的 `.new` 残渣.
    pub fn recover(&mut self) -> Result<()> {
        let mut manifest = Manifest::load_or_empty(&self.dir)?;
        if !manifest.settled() {
            self.apply(&manifest)?;
            manifest.promoted.clear();
            manifest.removed.clear();
            manifest.save(&self.dir)?;
        }

        // 迁移:还是 CSV 基表的那些表(TODO 7.5 之前建的库)
        let pending = self.tables_to_migrate(&manifest)?;
        if !pending.is_empty() {
            let persister = CsvPersister::new();
            for name in &pending {
                let at = TablePath::new(&self.dir, name);
                let table = persister.load(&at)?;
                // 写成 `<表>.heap.new`:推广要等提交记录写下去之后(见下面)
                let bytes = HeapTab::write_dense(&at.heap_path(), &table)?;
                manifest.tables.insert(
                    name.clone(),
                    TableFootprint {
                        heap: true,
                        heap_bytes: bytes,
                        heap_rows: table.nrows(),
                        moved: 0,
                    },
                );
            }
            // 提交点:过了这里,堆文件就是这些表的新家了
            manifest.promoted = pending.clone();
            manifest.save(&self.dir)?;
            self.apply(&manifest)?;
            manifest.promoted.clear();
            manifest.save(&self.dir)?;
            for name in &pending {
                self.remove_csv_files(name)?;
            }
        }
        // 已经迁完但 CSV 还留着(上次崩在删文件那一步):补删
        for (name, footprint) in &manifest.tables {
            if footprint.heap {
                self.remove_csv_files(name)?;
            }
        }

        // 死字节太多的堆表:**打开时**整理一次(这一层是唯一能安全重写文件的地方:
        // 此刻还没有人拿着这份文件读写,见 `pers::heap` 的文件头)
        self.compact_if_needed(&mut manifest)?;

        self.truncate_moved_logs(&manifest)?;
        self.sweep_staged(&manifest)?;
        self.manifest = manifest;
        Ok(())
    }

    /// 把死字节太多的页式堆表**密集重写**一遍(打开库时做,`&mut self` 收尾).
    ///
    /// 判据在 [`HeapTab::needs_compaction`]:文件比活着的字节多出一倍以上,而且
    /// 本身够大.重写 = 读出来 -> 写成 `<表>.heap.new` -> 清单记新位置(提交点)
    /// -> 推广 -> 位置日志归零.崩在中间只会留下一份 `.new`,下次再整理一次.
    fn compact_if_needed(&self, manifest: &mut Manifest) -> Result<()> {
        for (name, footprint) in manifest.tables.clone() {
            if !footprint.heap {
                continue;
            }
            let at = TablePath::new(&self.dir, &name);
            let (_, shape) = CsvPersister::new().load_schema(&at)?;
            let file = Rc::new(HeapFile::open(
                &at.heap_path(),
                footprint.heap_bytes,
                self.pool(),
            )?);
            let mut table = HeapTab::open(&name, shape, Rc::clone(&file), footprint.heap_rows)?;
            table.apply_moved_log(&at.log_path(), footprint.moved)?;
            if !table.needs_compaction() {
                continue;
            }

            let bytes = HeapTab::write_dense(&at.heap_path(), &table)?;
            let rows = table.nrows();
            let mut settled = manifest.clone();
            settled.tables.insert(
                name.clone(),
                TableFootprint {
                    heap: true,
                    heap_bytes: bytes,
                    heap_rows: rows,
                    moved: 0,
                },
            );
            settled.promoted = vec![name.clone()];
            settled.save(&self.dir)?;
            self.apply(&settled)?;
            truncate_moves(&at.log_path(), 0)?;

            *manifest = settled;
            manifest.promoted.clear();
            manifest.save(&self.dir)?;
        }
        Ok(())
    }

    /// 还没搬进堆文件的那些表(有 `<表>.csv`,而清单里没有它的堆位置).
    fn tables_to_migrate(&self, manifest: &Manifest) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("csv") {
                continue;
            }
            let stem = stem_of(&path);
            if manifest.tables.get(&stem).is_some_and(|at| at.heap) {
                continue;
            }
            out.push(stem);
        }
        out.sort();
        Ok(out)
    }

    /// 把每张表的行位置日志截到**已提交**的条数(清单里没有的表当 0).
    fn truncate_moved_logs(&self, manifest: &Manifest) -> Result<()> {
        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("log") {
                continue;
            }
            let stem = stem_of(&path);
            let committed = manifest.tables.get(&stem).map_or(0, |at| at.moved);
            truncate_moves(&path, committed)?;
        }
        Ok(())
    }

    /// 删掉一张表的老格式基表(`<表>.csv`).不在不算错.
    ///
    /// `<表>.log` **不删**:那个名字现在归**行位置日志**(迁移之后它是空的,接着用).
    fn remove_csv_files(&self, name: &str) -> Result<()> {
        let at = TablePath::new(&self.dir, name);
        if let Err(e) = fs::remove_file(at.data_path()) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(io_err(at.data_path(), e));
            }
        }
        Ok(())
    }

    /// 删掉没有提交记录庇护的暂存残渣(`*.csv.new` / `*.schema.json.new`).
    ///
    /// **必须在补完提交之后做**:清单里记着的那几个 `.new` 正是要搬进正式位置的
    /// 数据,先扫掉就把一次能补完的提交变成一次丢数据的提交了.
    fn sweep_staged(&self, manifest: &Manifest) -> Result<()> {
        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".csv.new")
                && !name.ends_with(".schema.json.new")
                && !name.ends_with(".heap.new")
            {
                continue;
            }
            let stem = name
                .trim_end_matches(".schema.json.new")
                .trim_end_matches(".heap.new")
                .trim_end_matches(".csv.new")
                .to_string();
            if manifest.promoted.contains(&stem) {
                continue;
            }
            let path = entry.path();
            fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
        }
        Ok(())
    }

    /// 删掉一张表的全部文件:堆文件,模式,行位置日志,以及老格式的 CSV 与它的日志.
    /// 这是 `DROP SCHEMA ... CASCADE` 的落点.文件不在不算错.
    pub fn remove_table_files(&self, name: &str) -> Result<()> {
        let name = checked_table_name(name)?;
        let at = TablePath::new(&self.dir, &name);
        let paths: [std::path::PathBuf; 4] = [
            at.heap_path(),
            at.schema_path().to_path_buf(),
            at.log_path(),
            at.data_path().to_path_buf(),
        ];
        for path in paths {
            if let Err(e) = fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(io_err(&path, e));
                }
            }
        }
        Ok(())
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // 句柄关掉时内核本来就会释放 flock;显式 unlock 只是把"锁的寿命 = 这个句柄"
        // 这件事写在代码里,别哪天顺手把 `lock` 字段去掉.
        let _ = self.lock.unlock();
    }
}

/// 一个路径的**文件名主干**(表名).
fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 库名合法性:只是**服务目录下的一个目录名**.
///
/// 空,`.`,`..`,含 `/` 或 `\` 或 NUL 都拒绝--否则 `CREATE DATABASE ../x`
/// 就跑到服务目录外面去了.
///
/// [`is_file_stem_safe`]: crate::db::table::is_file_stem_safe
fn checked_name(name: &str) -> Result<String> {
    if !is_file_stem_safe(name) {
        return Err(DbError::Denied {
            message: format!("{}", tr!("usr.service.019", arg0 = 
                name
            )),
        });
    }
    Ok(name.to_string())
}

/// 表名的同一道闸(见 [`Database::commit`]).
///
/// [`is_file_stem_safe`]: crate::db::table::is_file_stem_safe
fn checked_table_name(name: &str) -> Result<String> {
    if !is_file_stem_safe(name) {
        return Err(DbError::Denied {
            message: format!("{}", tr!("usr.service.020", arg0 = 
                name
            )),
        });
    }
    Ok(name.to_string())
}

fn io_err(path: &Path, e: std::io::Error) -> DbError {
    DbError::Io {
        path: path.display().to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}
