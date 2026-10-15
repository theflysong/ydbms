//! 库内的授权表(`<库>/grants.json`):**谁能连**,**谁能在哪张表的哪些列上做什么**.
//!
//! ```text
//! <库>/grants.json
//! { "version": 1,
//!   "connect": ["alice"],
//!   "tables": [ { "grantee": "alice", "table": "movies", "privilege": "SELECT",
//!                 "columns": ["title"] },
//!               { "grantee": "bob", "table": "movies", "privilege": "INSERT",
//!                 "columns": [] } ] }
//! ```
//!
//! # 三条口径
//!
//! * **一条记录 = (受权人, 表, 权限)**:`columns` 为空 = **整表**,非空 = **列级**
//!   (只授那几列).扁平的一条一条,好处是同一条语句里不同权限可以带不同的列清单
//!   (`GRANT SELECT (a), UPDATE (b) ON t`)--嵌套写法要为此再分一层.
//! * **`connect` 是库级的那一个权限**:非 owner / 非超级用户的人要连这个库,名字必须
//!   在这里.它不给任何表权限,表权限也不给连接的能力.
//! * **列名而不是列号**:本版没有 `ALTER TABLE`,列名稳定,而且 `grants.json` 是给人
//!   看的(手工检查一眼就懂).
//!
//! # 磁盘契约
//!
//! 缺文件 = **没有授权**(老库照旧能开);JSON 坏 / 版本不认识 = **报错**(与
//! `catalog.json` 同一条规矩:坏了就不许猜).写盘一律**临时文件 + rename**,
//! 而且**先规范化**:按 `(grantee, table, privilege, columns)` 排序去重,
//! 空记录删掉--于是同一份授权表只有一种写法,手工 diff 与测试都稳定.
//!
//! 它跟着库目录走,所以**受库的单写者锁保护**(`crate::usr::service`):本会话连着的
//! 那个库直接改手里的 [`Database`](crate::usr::service::Database);别的库要借一次它
//! 的锁(`ServiceDir::with_grants`)--拿不到就不改,宁可报错.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::table::{DbError, Result};

/// `grants.json` 的格式版本.
pub const GRANTS_VERSION: u32 = 1;

/// 授权表的文件名.
pub const GRANTS_FILE: &str = "grants.json";

/// 表级授权的三种权限(**本版只有这三种**:没有 `DELETE`,也没有 `TRUNCATE`).
///
/// 顺序就是写盘与显示的固定顺序,别随手改(它决定了规范化之后的文件长什么样).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Privilege {
    /// 读.
    Select,
    /// 插.
    Insert,
    /// 改.
    Update,
}

impl Privilege {
    /// 给用户看的那个词(也是盘上的那个词).
    pub fn as_str(&self) -> &'static str {
        match self {
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
        }
    }

    /// 三个全要(`GRANT ALL ...`).
    pub fn all() -> [Privilege; 3] {
        [Privilege::Select, Privilege::Insert, Privilege::Update]
    }
}

/// 一条表级授权.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableGrant {
    /// 受权人(角色名).
    pub grantee: String,
    /// 表名(生效名,与提交点用的是同一套名字).
    pub table: String,
    /// 授的是哪一种权限.
    pub privilege: Privilege,
    /// **空 = 整表**;非空 = 只有这几列.
    #[serde(default)]
    pub columns: Vec<String>,
}

/// `<库>/grants.json` 的内容.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grants {
    /// 格式版本,见 [`GRANTS_VERSION`].
    pub version: u32,
    /// 能连这个库的那些角色(**owner 与超级用户不在这里**:它们本来就进得来).
    #[serde(default)]
    pub connect: Vec<String>,
    /// 表级 / 列级授权.
    #[serde(default)]
    pub tables: Vec<TableGrant>,
}

impl Default for Grants {
    fn default() -> Self {
        Grants::empty()
    }
}

impl Grants {
    /// 空授权表(`version` 是当前版本).
    pub fn empty() -> Self {
        Grants {
            version: GRANTS_VERSION,
            connect: Vec::new(),
            tables: Vec::new(),
        }
    }

    /// 读库目录里的授权表;**缺文件就是 `None`**(不是"空授权表",也不是错误).
    ///
    /// JSON 坏了 / 版本不认识一律报错:授权表读不出来时**不能**当成"没有授权"
    /// --那等于把所有人的权限悄悄收回(表权限)或者悄悄放开(`CONNECT` 一空,
    /// 只剩 owner 与超级用户,方向是安全的,但"读不出来"这件事本身得说).
    pub fn load(dir: &Path) -> Result<Option<Grants>> {
        let path = dir.join(GRANTS_FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path, e)),
        };

        let grants: Grants = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.grants.001", arg0 =  e)),
        })?;
        if grants.version != GRANTS_VERSION {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("usr.grants.002", arg0 = 
                    grants.version, arg1 =  GRANTS_VERSION
                )),
            });
        }
        Ok(Some(grants))
    }

    /// 读一份;**缺文件**给空授权表(打开库时用:老库没有这个文件).
    pub fn load_or_empty(dir: &Path) -> Result<Grants> {
        Ok(Grants::load(dir)?.unwrap_or_default())
    }

    /// 写进库目录(临时文件 + rename),写之前**先规范化**.
    pub fn save(&mut self, dir: &Path) -> Result<()> {
        self.normalize();

        let path = dir.join(GRANTS_FILE);
        let json = serde_json::to_string_pretty(self).map_err(|e| DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: format!("{}", tr!("usr.grants.003", arg0 =  e)),
        })?;

        let tmp = dir.join(format!("{}.tmp", GRANTS_FILE));
        std::fs::write(&tmp, format!("{}\n", json)).map_err(|e| io_err(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io_err(&path, e))
    }

    /// 这个角色能不能连(只看授权表;owner 与超级用户由调用方先放行).
    pub fn may_connect(&self, role: &str) -> bool {
        self.connect.iter().any(|name| name == role)
    }

    /// `grantee` 在 `table` 的 `column` 这一列上有没有 `want`.
    ///
    /// 整表授权覆盖每一列,所以"找到一条 `columns` 为空的记录"就算有.
    pub fn allows(&self, grantee: &str, table: &str, want: Privilege, column: &str) -> bool {
        self.tables.iter().any(|grant| {
            grant.grantee == grantee
                && grant.table == table
                && grant.privilege == want
                && (grant.columns.is_empty() || grant.columns.iter().any(|held| held == column))
        })
    }

    /// `grantee` 在 `table` 上有没有 `want`,**不管哪一列**.
    ///
    /// 给"这条查询一列都没读到具体值"的情形用(`SELECT count(*) FROM t`):
    /// 那种查询仍要求这张表上**至少有这一种权限**(整表或任意一列),
    /// 而不是"什么都不用".
    pub fn allows_any(&self, grantee: &str, table: &str, want: Privilege) -> bool {
        self.tables
            .iter()
            .any(|grant| grant.grantee == grantee && grant.table == table && grant.privilege == want)
    }

    /// 授权表里提到过这个角色吗(`DROP ROLE` / 改名的"干净角色"检查用).
    pub fn mentions(&self, role: &str) -> bool {
        self.may_connect(role) || self.tables.iter().any(|grant| grant.grantee == role)
    }

    /// 授 / 收库级的 `CONNECT`.
    pub fn set_connect(&mut self, grantee: &str, allowed: bool) {
        let held = self.connect.iter().any(|name| name == grantee);
        match (allowed, held) {
            (true, false) => self.connect.push(grantee.to_string()),
            (false, true) => self.connect.retain(|name| name != grantee),
            _ => {}
        }
    }

    /// 授 / 收**一种**表级(或列级)权限.
    ///
    /// `columns` 为空 = 整表;非空 = 只动那几列.
    ///
    /// 两条与 PostgreSQL 的 ACL 分档一致的口径:
    ///
    /// * `REVOKE <权限> ON t`(不给列)**只删整表那条记录**,列级记录留着;
    /// * `REVOKE <权限> (列) ON t` 从列级记录里摘掉那几列,摘空了连记录一起删.
    ///
    /// (一种权限一次调用,是因为同一条语句里不同权限可以带不同的列清单:
    /// `GRANT SELECT (a), UPDATE (b) ON t`.)
    pub fn set_table(
        &mut self,
        table: &str,
        grantee: &str,
        privilege: Privilege,
        columns: &[String],
        allowed: bool,
    ) {
        if allowed {
            self.add_table(table, grantee, privilege, columns);
        } else {
            self.remove_table(table, grantee, privilege, columns);
        }
    }

    /// 授:`columns` 为空就并进整表那条记录,否则并进列级那条(没有就新建).
    fn add_table(&mut self, table: &str, grantee: &str, privilege: Privilege, columns: &[String]) {
        let same = |grant: &TableGrant| {
            grant.grantee == grantee
                && grant.table == table
                && grant.privilege == privilege
                && grant.columns.is_empty() == columns.is_empty()
        };

        if columns.is_empty() {
            if !self.tables.iter().any(same) {
                self.tables.push(TableGrant {
                    grantee: grantee.to_string(),
                    table: table.to_string(),
                    privilege,
                    columns: Vec::new(),
                });
            }
            return;
        }

        match self.tables.iter_mut().find(|grant| same(grant)) {
            Some(held) => {
                for column in columns {
                    if !held.columns.iter().any(|name| name == column) {
                        held.columns.push(column.clone());
                    }
                }
            }
            None => self.tables.push(TableGrant {
                grantee: grantee.to_string(),
                table: table.to_string(),
                privilege,
                columns: columns.to_vec(),
            }),
        }
    }

    /// 收:`columns` 为空就删整表那条,否则从列级那条里摘列.
    fn remove_table(
        &mut self,
        table: &str,
        grantee: &str,
        privilege: Privilege,
        columns: &[String],
    ) {
        self.tables.retain_mut(|grant| {
            if grant.grantee != grantee || grant.table != table || grant.privilege != privilege {
                return true;
            }
            if columns.is_empty() {
                // 只删**整表**那条:列级记录留着(与 PostgreSQL 的 ACL 分档一致)
                return !grant.columns.is_empty();
            }
            // 列级那条:摘掉这几列,摘空了连记录一起删
            grant.columns.retain(|name| !columns.iter().any(|drop| drop == name));
            !grant.columns.is_empty()
        });
    }

    /// `DROP OWNED BY`:把这个角色名下的授权全部收回(连接与表权限都收).
    pub fn set_owned_away(&mut self, role: &str) {
        self.connect.retain(|name| name != role);
        self.tables.retain(|grant| grant.grantee != role);
    }

    /// 这些表已经没了:把它们名下的授权记录一并删掉(返回有没有动过).
    pub fn prune_tables(&mut self, tables: &[String]) -> bool {
        let before = self.tables.len();
        self.tables
            .retain(|grant| !tables.iter().any(|gone| gone == &grant.table));
        before != self.tables.len()
    }

    /// 规范化:排序,去重,空的列级记录删掉.
    ///
    /// 于是"同一份授权"只有一种落盘写法--手工 diff 与测试都稳定,也不会因为
    /// 反复 `GRANT` 同一条而把文件撑大.
    pub fn normalize(&mut self) {
        self.connect.sort();
        self.connect.dedup();

        for grant in &mut self.tables {
            grant.columns.sort();
            grant.columns.dedup();
        }

        // 同一个人在同一张表上的同一种权限,列级记录**归并成一条**(手工编辑出两条
        // 也能读,但写回去只有一条);整表那条是独立的(它不是"所有列的并集",
        // 语义上覆盖以后的列,所以不与列级混在一起)
        let mut merged: Vec<TableGrant> = Vec::with_capacity(self.tables.len());
        for grant in self.tables.drain(..) {
            if grant.columns.is_empty() {
                if !merged.iter().any(|held| *held == grant) {
                    merged.push(grant);
                }
                continue;
            }
            match merged.iter_mut().find(|held| {
                held.grantee == grant.grantee
                    && held.table == grant.table
                    && held.privilege == grant.privilege
                    && !held.columns.is_empty()
            }) {
                Some(held) => {
                    for column in grant.columns {
                        if !held.columns.contains(&column) {
                            held.columns.push(column);
                        }
                    }
                    held.columns.sort();
                    held.columns.dedup();
                }
                None => merged.push(grant),
            }
        }
        self.tables = merged;

        self.tables.sort_by(|a, b| {
            (&a.grantee, &a.table, a.privilege, &a.columns).cmp(&(
                &b.grantee,
                &b.table,
                b.privilege,
                &b.columns,
            ))
        });
        self.tables.dedup();
    }
}

fn io_err(path: &Path, e: std::io::Error) -> DbError {
    DbError::Io {
        path: path.display().to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}
