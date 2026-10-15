//! 服务目录:用户表与所有库放在哪儿,一个库目录长什么样,谁能连.
//!
//! ```text
//! <server-dist>/                     默认 ~/.ydbms(--server-dist 可换)
//! ├── users.json                    用户表(见 [`crate::usr`])
//! └── <库>/
//!     ├── catalog.json              {version, name, owner, created_unix, settings}
//!     ├── .lock                     单写者锁(flock;进程退出即释放)
//!     ├── commit.json               提交意图:崩在中途时靠它把提交补完
//!     ├── <表>.csv                  数据(表名带 schema 前缀时就是文件名)
//!     └── <表>.schema.json          模式(格式归 [`crate::pers`])
//! ```
//!
//! **一个目录 = 一个库**,这与 [`crate::pers`] 的既有约定一致,也方便手工检查.
//! 表文件的读写规则**只在 [`crate::pers`] 一处**:这里只负责"库有哪些表,
//! 谁建的,现在能不能打开",一次 `TablePath::new(dir, 表名)` 就把两者接上.
//!
//! # `catalog.json` 是磁盘契约
//!
//! `owner` 就是 [`crate::usr::User::name`];`version` 不认识就报错.**缺文件不自动补**
//! --owner 无从知道,补一个等于伪造 owner,宁可不连.这也是"只有
//! `CREATE DATABASE` 会建出库目录"这句话的落点.`settings`(见 [`Settings`])是
//! 库级的会话默认值,**新增字段一律 `#[serde(default)]`**,老文件照旧读得回来.
//!
//! # 提交是原子的(4.2)
//!
//! 一次提交要写好几张表,而每张表是"成对的两个文件各自 rename"--崩在中间就会留下
//! 改了一半的库.所以先写**暂存**文件(`<表>.csv.new`),全写完了才落下
//! [`CommitIntent`](`commit.json`),再逐个 rename 进正式位置,删掉要删的表.
//! **打开库时**([`Database::recover`])看到 `commit.json` 就把它补完,看到没有
//! 意图庇护的 `.new` 就清掉--两次崩溃之间,库要么全在提交前,要么全在提交后.
//! 不做 WAL,也不 `fsync`:这条保证只覆盖**进程崩溃**(含 `kill -9`),不覆盖掉电.
//!
//! # 单写者锁
//!
//! 每个库目录上放一把 `flock`([`std::fs::File::try_lock`],Rust 1.89 起是标准库
//! 能力,**不用加依赖**).本版**不做多进程协调**:两个写者会互相踩,所以第二个
//! 进程直接**连不上**--宁可连不上,也不要写坏数据.锁是随句柄走的,进程退出
//! (哪怕被 `kill -9`)内核就释放,没有"陈旧锁文件"要清理.恢复也要在锁下做
//! ([`ServiceDir::open`] 里先上锁,再恢复).

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{now_unix, User};
use crate::db::phys::PhysTab;
use crate::db::table::{is_file_stem_safe, DbError, Result, Table};
use crate::pers::csv::promote;
use crate::pers::{CsvPersister, Persister, TablePath};

/// `catalog.json` 的格式版本.
pub const CATALOG_VERSION: u32 = 1;

/// 库元数据的文件名.
pub const CATALOG_FILE: &str = "catalog.json";

/// 单写者锁的文件名.
pub const LOCK_FILE: &str = ".lock";

/// 提交意图的文件名(见 [`CommitIntent`]).
pub const COMMIT_FILE: &str = "commit.json";

/// `commit.json` 的格式版本.
pub const COMMIT_VERSION: u32 = 1;

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

/// 一次多表提交的**意图**(`<库>/commit.json`).
///
/// 一次提交要把好几张表写回磁盘,而每张表是"成对两个文件各自 rename"--崩在中途
/// 就会留下"改了一半"的库.所以提交分三段:
///
/// 1. 把每一张表都写成**暂存**文件(`<表>.csv.new` / `<表>.schema.json.new`);
/// 2. 把这份意图写进 `commit.json` -- **过了这一点,这次提交就必须完成**;
/// 3. 逐张把暂存文件 rename 进正式位置,删掉要删的表,最后删掉 `commit.json`.
///
/// 于是启动时只剩两种可能:**没有** `commit.json`(那次提交还没越过第 2 步,
/// 磁盘上完全是提交前的那一份,暂存残渣清扫掉即可)或**有**(新数据早已全部落盘,
/// 按它补完就是提交后的那一份).**两种都不会是"改了一半"**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitIntent {
    /// 格式版本,见 [`COMMIT_VERSION`].
    pub version: u32,
    /// 这次要写的表(生效名)--它们的暂存文件在写下这份意图时**已经就绪**.
    pub written: Vec<String>,
    /// 这次要删掉文件的表(`DROP SCHEMA ... CASCADE` 的落点).
    pub removed: Vec<String>,
}

impl CommitIntent {
    /// 读库目录里的提交意图;没有就是 `None`.
    ///
    /// **读到坏的,或者版本不认识的,一律报错**:宁可开不了库,也不要静默地把
    /// "上次提交崩在中途"这件事当成没发生(那正是这份文件存在的唯一理由).
    pub fn load(dir: &Path) -> Result<Option<CommitIntent>> {
        let path = dir.join(COMMIT_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path, e)),
        };

        let intent: CommitIntent = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.service.005", arg0 = 
                e
            )),
        })?;
        if intent.version != COMMIT_VERSION {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("usr.service.006", arg0 = 
                    intent.version, arg1 =  COMMIT_VERSION
                )),
            });
        }
        Ok(Some(intent))
    }

    /// 写进**库目录**(临时文件 + rename,与 [`Catalog::save`] 同一套).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(COMMIT_FILE);
        let json = serde_json::to_string_pretty(self).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.service.007", arg0 =  e)),
        })?;

        let tmp = dir.join(format!("{}.tmp", COMMIT_FILE));
        fs::write(&tmp, format!("{}\n", json)).map_err(|e| io_err(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }

    /// 删掉提交意图(提交补完之后).不在不算错.
    pub fn remove(dir: &Path) -> Result<()> {
        let path = dir.join(COMMIT_FILE);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&path, e)),
        }
    }
}

/// 服务目录:`users.json` 与各个库的父目录.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDir {
    root: PathBuf,
}

impl ServiceDir {
    /// 定下服务目录:给了 `root` 就用它,否则 `$HOME/.ydbms`.
    ///
    /// `home` 由调用方从环境里取(不在这儿读环境,于是可测).**没有 `HOME` 也不给
    /// `--server-dist` 就报错**--猜一个目录出来比报错糟糕得多.首次使用会自己把
    /// 目录树建出来.
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

        fs::create_dir_all(&root).map_err(|e| io_err(&root, e))?;
        Ok(ServiceDir { root })
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
        if catalog.owner != user.name && !user.is_admin {
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

        let database = Database {
            dir,
            name,
            catalog,
            lock,
        };

        // 上次崩在提交中途:**拿到锁之后,谁来读表之前**把它补完(见
        // [`Database::recover`]).放在这里而不是 `load_tables` 里,是因为恢复要
        // 改文件--必须在锁下做.
        database.recover()?;
        Ok(database)
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

    /// 把库里所有表读回内存.
    ///
    /// 扫 `*.csv`(暂存文件叫 `*.csv.new`,扩展名不是 `csv`,扫不到;写一半的残渣
    /// 由 [`Database::recover`] 在打开时清掉),逐个
    /// [`Persister::load`],并**校验文件主干名与模式里的表名一致**--不一致意味着
    /// 有人手工动过目录,继续用会在下一次保存时写出两份文件,留下幽灵表.
    pub fn load_tables(&self) -> Result<Vec<PhysTab>> {
        let persister = CsvPersister::new();
        let mut paths = Vec::new();

        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("csv") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut tables = Vec::with_capacity(paths.len());
        for path in paths {
            let table = persister.load(&TablePath::from_data_path(&path))?;
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if stem != table.name() {
                return Err(DbError::Format {
                    path: path.display().to_string(),
                    line: None,
                    message: format!("{}", tr!("usr.service.018", arg0 = 
                        stem, arg1 = 
                        table.name()
                    )),
                });
            }
            tables.push(table);
        }
        Ok(tables)
    }

    /// 一次**原子**的多表提交(4.2):写 `tables`,删 `removed`,两者要么全做,
    /// 要么全不做.协议见 [`CommitIntent`].
    ///
    /// 中途失败(IO 错,进程被杀)都只会停在三个位置之一,而这三种在重启时都有
    /// 确定的去向:暂存阶段(没写意图)→ 全在提交前;写了意图之后 → 补完全在提交后;
    /// **没有"改了一半"这种状态**.
    ///
    /// 表名**就是文件名主干**,所以这里是"名字能不能当文件名"的最后一道闸
    /// (见 [`crate::db::table::is_file_stem_safe`]):`ir` 建表时已经拒过一次,
    /// 但真正碰文件系统的是这里--**这道闸不能省**,名字里带 `../` 真的会写到
    /// 别人的库目录(甚至服务目录)里去.
    pub fn commit(&self, tables: &[&dyn Table], removed: &[String]) -> Result<()> {
        let persister = CsvPersister::new();

        // 1) 先把每一张表都写成暂存文件.这一步失败就什么都还没提交:`.new` 是
        //    孤儿残渣(下次提交覆盖它,恢复时清扫掉).
        let mut written = Vec::with_capacity(tables.len());
        for table in tables {
            let name = checked_table_name(table.name())?;
            persister.stage(*table, &TablePath::new(&self.dir, &name))?;
            written.push(name);
        }

        // 2) 提交意图落盘 -- **过了这一点,这次提交就必须完成**
        let intent = CommitIntent {
            version: COMMIT_VERSION,
            written,
            removed: removed.to_vec(),
        };
        intent.save(&self.dir)?;

        // 3) 补完.重放([`Database::recover`])走的是同一条路.
        self.replay(&intent)
    }

    /// 把一个提交意图做到底:推广暂存文件,删掉要删的表,最后删掉意图本身.
    ///
    /// **幂等**:已经搬过的暂存文件(rename 掉之后 `.new` 就没了)会被跳过,
    /// 删掉的文件再删一次也不算错--所以它可以被"崩在中途"之后的恢复直接重跑.
    fn replay(&self, intent: &CommitIntent) -> Result<()> {
        for name in &intent.written {
            let name = checked_table_name(name)?;
            promote(&TablePath::new(&self.dir, &name))?;
        }
        for name in &intent.removed {
            self.remove_table_files(name)?;
        }
        CommitIntent::remove(&self.dir)
    }

    /// 打开库时把上次没写完的提交收尾(4.2),并清扫暂存残渣.
    ///
    /// 看到 `commit.json` 就**补完**它(新数据早就在暂存文件里了,这是"全在提交后");
    /// 没有 `commit.json` 而目录里还留着 `.new`,那是**暂存到一半**留下的--删掉
    /// 就等于回到提交前(这是"全在提交前").两条路都不会留下"改了一半"的库.
    pub fn recover(&self) -> Result<()> {
        if let Some(intent) = CommitIntent::load(&self.dir)? {
            self.replay(&intent)?;
        }
        self.sweep_staged()
    }

    /// 删掉没有提交意图庇护的暂存残渣(`*.csv.new` / `*.schema.json.new`).
    ///
    /// **必须在补完提交之后做**:`commit.json` 还留着的时候,那些 `.new` 正是要搬
    /// 进正式位置的数据,先扫掉就把一次能补完的提交变成一次丢数据的提交了.
    fn sweep_staged(&self) -> Result<()> {
        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".csv.new") || name.ends_with(".schema.json.new") {
                let path = entry.path();
                fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            }
        }
        Ok(())
    }

    /// 删掉一张表的两个文件(`DROP SCHEMA ... CASCADE` 的落点).文件不在不算错.
    pub fn remove_table_files(&self, name: &str) -> Result<()> {
        let name = checked_table_name(name)?;
        let at = TablePath::new(&self.dir, &name);
        for path in [at.data_path(), at.schema_path()] {
            if let Err(e) = fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(io_err(path, e));
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
