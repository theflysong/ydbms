//! 服务目录：用户表与所有库放在哪儿、一个库目录长什么样、谁能连。
//!
//! ```text
//! <server-dist>/                     默认 ~/.ydbms（--server-dist 可换）
//! ├── users.json                    用户表（见 [`crate::usr`]）
//! └── <库>/
//!     ├── catalog.json              {version, name, owner, created_unix}
//!     ├── .lock                     单写者锁（flock；进程退出即释放）
//!     ├── <表>.csv                  数据（表名带 schema 前缀时就是文件名）
//!     └── <表>.schema.json          模式（格式归 [`crate::pers`]）
//! ```
//!
//! **一个目录 = 一个库**，这与 [`crate::pers`] 的既有约定一致，也方便手工检查。
//! 表文件的读写规则**只在 [`crate::pers`] 一处**：这里只负责"库有哪些表、
//! 谁建的、现在能不能打开"，一次 `TablePath::new(dir, 表名)` 就把两者接上。
//!
//! # `catalog.json` 是磁盘契约
//!
//! `owner` 就是 [`crate::usr::User::name`]；`version` 不认识就报错。**缺文件不自动补**
//! ——owner 无从知道，补一个等于伪造 owner，宁可不连。这也是"只有
//! `CREATE DATABASE` 会建出库目录"这句话的落点。
//!
//! # 单写者锁
//!
//! 每个库目录上放一把 `flock`（[`std::fs::File::try_lock`]，Rust 1.89 起是标准库
//! 能力，**不用加依赖**）。本版**不做多进程协调**：两个写者会互相踩，所以第二个
//! 进程直接**连不上**——宁可连不上，也不要写坏数据。锁是随句柄走的，进程退出
//! （哪怕被 `kill -9`）内核就释放，没有"陈旧锁文件"要清理。

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{now_unix, User};
use crate::db::phys::PhysTab;
use crate::db::table::{is_file_stem_safe, DbError, Result, Table};
use crate::pers::{CsvPersister, Persister, TablePath};

/// `catalog.json` 的格式版本。
pub const CATALOG_VERSION: u32 = 1;

/// 库元数据的文件名。
pub const CATALOG_FILE: &str = "catalog.json";

/// 单写者锁的文件名。
pub const LOCK_FILE: &str = ".lock";

/// 一个库的元数据（`<库>/catalog.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// 格式版本，见 [`CATALOG_VERSION`]。
    pub version: u32,
    /// 库名（就是目录名）。
    #[serde(default)]
    pub name: String,
    /// owner 的用户名——只有他（或管理员）能连这个库。
    pub owner: String,
    /// 建立时间（unix 秒）。
    #[serde(default)]
    pub created_unix: u64,
}

impl Catalog {
    /// 从**库目录**读元数据。
    pub fn load(dir: &Path) -> Result<Catalog> {
        let path = dir.join(CATALOG_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DbError::Format {
                    path: path.display().to_string(),
                    line: None,
                    message: "缺 catalog.json：这个目录不是本版建的库（只有 CREATE DATABASE 会建它）"
                        .to_string(),
                })
            }
            Err(e) => return Err(io_err(&path, e)),
        };

        let catalog: Catalog = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("catalog.json 不合法：{}", e),
        })?;

        if catalog.version != CATALOG_VERSION {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!(
                    "不认识的 catalog.json 版本 {}（本版只认 {}）",
                    catalog.version, CATALOG_VERSION
                ),
            });
        }
        Ok(catalog)
    }

    /// 写进**库目录**（临时文件 + rename）。
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CATALOG_FILE);
        let json = serde_json::to_string_pretty(self).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("库元数据序列化失败：{}", e),
        })?;

        let tmp = dir.join(format!("{}.tmp", CATALOG_FILE));
        fs::write(&tmp, format!("{}\n", json)).map_err(|e| io_err(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }
}

/// 服务目录：`users.json` 与各个库的父目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDir {
    root: PathBuf,
}

impl ServiceDir {
    /// 定下服务目录：给了 `root` 就用它，否则 `$HOME/.ydbms`。
    ///
    /// `home` 由调用方从环境里取（不在这儿读环境，于是可测）。**没有 `HOME` 也不给
    /// `--server-dist` 就报错**——猜一个目录出来比报错糟糕得多。首次使用会自己把
    /// 目录树建出来。
    pub fn resolve(root: Option<PathBuf>, home: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(root) => root,
            None => match home {
                Some(home) => home.join(".ydbms"),
                None => {
                    return Err(DbError::Denied {
                        message: "没有 --server-dist，也取不到 $HOME：请显式给出服务目录".to_string(),
                    })
                }
            },
        };

        if root.as_os_str().is_empty() {
            return Err(DbError::Denied {
                message: "服务目录不能为空".to_string(),
            });
        }

        fs::create_dir_all(&root).map_err(|e| io_err(&root, e))?;
        Ok(ServiceDir { root })
    }

    /// 服务目录的根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `users.json` 的路径。
    pub fn users_path(&self) -> PathBuf {
        self.root.join("users.json")
    }

    /// 某个库的目录。
    pub fn database_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// 这个库在不在（目录在、且 `catalog.json` 在）。
    pub fn exists(&self, name: &str) -> bool {
        self.root.join(name).join(CATALOG_FILE).is_file()
    }

    /// 服务目录下所有库的名字（按字典序）。
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

    /// 读一个库的元数据。
    pub fn catalog(&self, name: &str) -> Result<Catalog> {
        let name = checked_name(name)?;
        Catalog::load(&self.database_dir(&name))
    }

    /// `CREATE DATABASE name`：建目录、写元数据。返回给用户看的一行状态。
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
        }
        .save(&dir)?;

        Ok(format!("库 {} 已建（owner {}）", name, owner))
    }

    /// `DROP DATABASE [IF EXISTS] name`：删掉整个库目录。返回一行状态。
    ///
    /// 别人正连着这个库就**不删**（先探一次单写者锁）：删掉一个有人正在写的库，
    /// 比让他连不上糟糕得多。
    pub fn drop_database(&self, name: &str, if_exists: bool) -> Result<String> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);

        if !dir.join(CATALOG_FILE).is_file() {
            return if if_exists {
                Ok(format!("库 {} 不存在（IF EXISTS）", name))
            } else {
                Err(DbError::UnknownDatabase { name })
            };
        }

        let probe = File::create(dir.join(LOCK_FILE)).map_err(|e| io_err(&dir, e))?;
        match probe.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DbError::Denied {
                    message: format!("库 `{}` 正被另一个进程使用，不能删", name),
                })
            }
            Err(TryLockError::Error(e)) => return Err(io_err(&dir, e)),
        }
        drop(probe);

        fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        Ok(format!("库 {} 已删", name))
    }

    /// 打开一个库：owner 检查 → 上单写者锁 → 返回句柄。
    ///
    /// 顺序要紧：**先上锁再让调用方载入表**，否则会在载入的半途被另一个写者改文件。
    pub fn open(&self, name: &str, user: &User) -> Result<Database> {
        let name = checked_name(name)?;
        let dir = self.database_dir(&name);

        if !dir.is_dir() {
            return Err(DbError::UnknownDatabase { name });
        }

        // 目录在、`catalog.json` 不在：说清是"这不是本版建的库"，而不是含糊的"库不存在"
        let catalog = Catalog::load(&dir)?;
        if catalog.owner != user.name && !user.is_admin {
            return Err(DbError::Denied {
                message: format!(
                    "库 `{}` 的 owner 是 `{}`，`{}` 不是管理员，连不上（本版没有 GRANT）",
                    name, catalog.owner, user.name
                ),
            });
        }

        let lock_path = dir.join(LOCK_FILE);
        let lock = File::create(&lock_path).map_err(|e| io_err(&lock_path, e))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DbError::Denied {
                    message: format!("库 `{}` 正被另一个进程使用（单写者锁）", name),
                })
            }
            Err(TryLockError::Error(e)) => return Err(io_err(&lock_path, e)),
        }

        Ok(Database {
            dir,
            name,
            catalog,
            lock,
        })
    }
}

/// 一个打开的库：目录、元数据、单写者锁。
///
/// 句柄活着的时候别人连不上同一个库；它也顺手管住表文件的读写（规则在
/// [`crate::pers`]，这里只是把"库目录 + 表名"接起来）。
#[derive(Debug)]
pub struct Database {
    dir: PathBuf,
    name: String,
    catalog: Catalog,
    /// 单写者锁。**只是持有**：句柄关掉（含进程退出、被 `kill -9`）内核就释放。
    lock: File,
}

impl Database {
    /// 库名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 库目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// owner 的用户名。
    pub fn owner(&self) -> &str {
        &self.catalog.owner
    }

    /// 库元数据。
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// 把库里所有表读回内存。
    ///
    /// 扫 `*.csv`（写一半的临时文件叫 `*.csv.tmp`，扫不到），逐个
    /// [`Persister::load`]，并**校验文件主干名与模式里的表名一致**——不一致意味着
    /// 有人手工动过目录，继续用会在下一次保存时写出两份文件、留下幽灵表。
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
                    message: format!(
                        "文件主干名 `{}` 与模式里的表名 `{}` 不一致（表名就是文件名主干）",
                        stem,
                        table.name()
                    ),
                });
            }
            tables.push(table);
        }
        Ok(tables)
    }

    /// 把一张表写进库目录。
    ///
    /// 表名**就是文件名主干**，所以这里是"名字能不能当文件名"的最后一道闸：
    /// `ir` 建表时已经拒过一次（见 [`crate::db::table::is_file_stem_safe`]），
    /// 但真正碰文件系统的是这里——**这道闸不能省**，名字里带 `../` 真的会写到
    /// 别人的库目录（甚至服务目录）里去。
    pub fn save_table(&self, table: &dyn Table) -> Result<()> {
        let name = checked_table_name(table.name())?;
        CsvPersister::new().save(table, &TablePath::new(&self.dir, &name))
    }

    /// 删掉一张表的两个文件（`DROP SCHEMA … CASCADE` 的落点）。文件不在不算错。
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
        // 句柄关掉时内核本来就会释放 flock；显式 unlock 只是把"锁的寿命 = 这个句柄"
        // 这件事写在代码里，别哪天顺手把 `lock` 字段去掉。
        let _ = self.lock.unlock();
    }
}

/// 库名合法性：只是**服务目录下的一个目录名**。
///
/// 空、`.`、`..`、含 `/` 或 `\` 或 NUL 都拒绝——否则 `CREATE DATABASE ../x`
/// 就跑到服务目录外面去了。
///
/// [`is_file_stem_safe`]: crate::db::table::is_file_stem_safe
fn checked_name(name: &str) -> Result<String> {
    if !is_file_stem_safe(name) {
        return Err(DbError::Denied {
            message: format!(
                "库名 `{}` 不能当目录名用（空、`.`、`..`、含 `/` 或 `\\` 都不行）",
                name
            ),
        });
    }
    Ok(name.to_string())
}

/// 表名的同一道闸（见 [`Database::save_table`]）。
///
/// [`is_file_stem_safe`]: crate::db::table::is_file_stem_safe
fn checked_table_name(name: &str) -> Result<String> {
    if !is_file_stem_safe(name) {
        return Err(DbError::Denied {
            message: format!(
                "表名 `{}` 不能当文件名用（空、`.`、`..`、含 `/` 或 `\\` 都不行）",
                name
            ),
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
