//! 符号表：名字 → 符号的映射。
//!
//! 它是整个系统里"名字"的唯一权威。设计上要承接两件事：
//!
//! * **数据库加载时批量导入**：持久化层把表读回来之后，一次性
//!   [`SymbolTable::extend`] 进符号表，之后 `Scan` 就能按名字取到它。
//! * **`AS` 之类的构造往表里加东西**：给已有符号起个别名——
//!   这就是 [`SymbolTable::alias`]，`Symbol::Alias` 是它的形状。
//!
//! 符号表**持有表实例**（`Box<dyn Table>`），而不只是形状，所以 `Scan` 按名字
//! 就能直接拿到数据；形状则通过 [`crate::db::table::TabShape`] 从表上取。
//!
//! # 索引也在这里（3.6）
//!
//! `CREATE INDEX` 建出来的索引是**目录对象**：[`IndexSymbol`] 记着名字、表与
//! 一个 [`IndexSlot`]（「列 + 当前那一份索引」）。读写路径都由它兜住——
//! `INSERT` 追加、`UPDATE` 作废、`DROP INDEX` 删掉、`DROP SCHEMA ... CASCADE`
//! 连同表一起清。索引**不落盘**。

use std::fmt;
use std::rc::Rc;

use crate::db::index::IndexSlot;
use crate::db::table::{Column, DbError, Result, Row, Table, Value};

/// 目录里的一条索引：`CREATE INDEX name ON table (column)`。
///
/// 名字与表名都是**生效名**（`search_path` 加过前缀的那种，与 `CREATE TABLE`
/// 同一套规则），所以查的时候要用 [`SymbolTable::effective_name`] 换算。
#[derive(Debug)]
pub struct IndexSymbol {
    /// 索引名（生效名）。
    pub name: String,
    /// 它建在哪张表上（生效名）。
    pub table: String,
    /// 「列 + 当前那一份索引」。执行器与优化器都借它。
    pub slot: Rc<IndexSlot>,
}

impl IndexSymbol {
    /// 索引建在哪一列。
    pub fn column(&self) -> usize {
        self.slot.column()
    }
}

/// 符号表里的一条记录。
pub enum Symbol {
    /// 一个关系：限定名 + 数据（数据同时提供形状）。
    Relation {
        /// 限定名，例如 `study.people`。
        name: String,
        /// 绑定的表。
        table: Box<dyn Table>,
    },
    /// 别名：新名字指向一个**已经存在**的名字。
    ///
    /// 这就是将来 `AS` 的落点。
    Alias {
        /// 别名。
        name: String,
        /// 被指向的符号名。
        target: String,
    },
}

impl Symbol {
    /// 造一个关系符号。接受 `impl Table` 省掉调用方手写 `Box::new`。
    pub fn relation<T: Table + 'static>(name: impl Into<String>, table: T) -> Self {
        Symbol::Relation {
            name: name.into(),
            table: Box::new(table),
        }
    }

    /// 造一个别名符号。
    pub fn alias(alias: impl Into<String>, target: impl Into<String>) -> Self {
        Symbol::Alias {
            name: alias.into(),
            target: target.into(),
        }
    }

    /// 符号的名字。
    pub fn name(&self) -> &str {
        match self {
            Symbol::Relation { name, .. } => name,
            Symbol::Alias { name, .. } => name,
        }
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::Relation { name, table } => f
                .debug_struct("Relation")
                .field("name", name)
                .field("columns", &table.columns())
                .finish(),
            Symbol::Alias { name, target } => f
                .debug_struct("Alias")
                .field("name", name)
                .field("target", target)
                .finish(),
        }
    }
}

/// 名字 → 符号的符号表。
///
/// 内部是一个插入有序的 `Vec`，线性查找。符号数量是个位数到几十，线性扫描比
/// 哈希表更快也更好读，而且顺序稳定、打印可预期。要换成 map 只是改这里。
///
/// # schema 与 `search_path`
///
/// 本版**没有完整的 schema 系统**：`study.movies` 这种限定名一直是被当作
/// **不透明的标识符**拼起来用的。这里补上的是两件配套的小事：
///
/// * `CREATE SCHEMA` 只是把名字**记下来**（[`SymbolTable::create_schema`]）；
/// * `SET search_path` 是真的生效的（[`SymbolTable::set_search_path`]）：
///   建表时非限定名按 `path[0]` 加前缀，解析名字时按 `path` 顺序试、
///   最后试裸名字。
///
/// 为什么后者不能只是"接受但无操作"：`filmdb.sql` 的建表写的是非限定名，
/// 而测试查的是 `study.movies`——不作前缀那些表就永远不存在。
#[derive(Debug, Default)]
pub struct SymbolTable {
    symbols: Vec<Symbol>,
    /// `CREATE INDEX` 建出来的索引，按建表顺序。名字与表名都是生效名。
    ///
    /// **不落盘**：它是会话级的目录对象（重启就没了），见
    /// [`crate::db::index`]。
    indexes: Vec<IndexSymbol>,
    /// `CREATE SCHEMA` 登记过的 schema 名（插入有序）。
    schemas: Vec<String>,
    /// 当前 `search_path`；**空**表示不加前缀（默认，行为与过去完全一致）。
    search_path: Vec<String>,
}

impl SymbolTable {
    /// 空表。
    pub fn new() -> Self {
        SymbolTable {
            symbols: Vec::new(),
            indexes: Vec::new(),
            schemas: Vec::new(),
            search_path: Vec::new(),
        }
    }

    /// 注册一个关系。重名 → [`DbError::DuplicateSymbol`]。
    ///
    /// 非限定名会按 `search_path` 的**第一个** schema 加前缀（见
    /// [`SymbolTable::effective_name`]）。
    pub fn define<T: Table + 'static>(
        &mut self,
        name: impl Into<String>,
        table: T,
    ) -> Result<()> {
        let name = self.effective_name(&name.into());
        self.extend([Symbol::relation(name, table)])
    }

    /// 批量导入。
    ///
    /// 这就是"数据库被加载时初始化一批符号"的入口：持久化层读回一批表之后，
    /// 一次调用把它们全部装进符号表。整批是**原子的**——任何一条重名就整体失败，
    /// 不会留下装了一半的符号表。
    pub fn extend(&mut self, symbols: impl IntoIterator<Item = Symbol>) -> Result<()> {
        let mut pending: Vec<Symbol> = Vec::new();

        for symbol in symbols {
            let name = symbol.name().to_string();
            let clash = self.symbols.iter().any(|s| s.name() == name)
                || pending.iter().any(|s| s.name() == name);
            if clash {
                return Err(DbError::DuplicateSymbol { name });
            }
            pending.push(symbol);
        }

        self.symbols.extend(pending);
        Ok(())
    }

    /// 给一个**已经存在**的名字起别名（`AS` 的落点）。
    ///
    /// 目标必须已存在，且别名不能重名——这两条保证别名链天然无环，不需要环检测。
    pub fn alias(&mut self, alias: impl Into<String>, target: &str) -> Result<()> {
        if self.lookup(target).is_none() {
            return Err(DbError::UnknownSymbol {
                name: target.to_string(),
            });
        }
        self.extend([Symbol::alias(alias, target)])
    }

    /// 名字 → 符号（不穿透别名）。
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.symbol_index(name).map(|index| &self.symbols[index])
    }

    /// 名字 → 关系（**穿透别名**）。
    pub fn relation(&self, name: &str) -> Option<&dyn Table> {
        match self.resolve(name)? {
            Symbol::Relation { table, .. } => Some(&**table),
            // resolve 只会返回 Relation，这里只是让类型穷尽
            Symbol::Alias { .. } => None,
        }
    }

    /// 名字 → 关系，但**按存进来的名字精确取**：不认 `search_path`、不穿别名。
    ///
    /// 与 [`relation`](SymbolTable::relation) 的差别只在一处：名字**不**按当前
    /// `search_path` 展开。提交点（`Session::commit`）必须用这一个——脏表记的是
    /// **这张表自己的生效名**，若按当时的 `search_path` 再解一次，就可能解到另一张
    /// 同名的表上（`s.t` 与 `t` 都在、`search_path` 指向 `s`），于是**改了 A 却存了 B**、
    /// 真正的 A 悄悄地没落盘。
    pub fn relation_exact(&self, name: &str) -> Option<&dyn Table> {
        self.symbols.iter().find_map(|symbol| match symbol {
            Symbol::Relation { name: stored, table } if stored == name => Some(&**table),
            _ => None,
        })
    }

    /// 名字 → **可变**的表句柄（穿透别名）。
    ///
    /// `INSERT` 要改表内容，而 [`relation`](SymbolTable::relation) 是只读的。
    /// 实现上分两步：先用只读借用解出下标，再按下标取 `&mut`——这也是为什么
    /// 别名穿透要有一个返回下标的版本。
    pub fn relation_mut(&mut self, name: &str) -> Option<&mut Box<dyn Table>> {
        let index = self.resolve_index(name)?;
        match self.symbols.get_mut(index)? {
            Symbol::Relation { table, .. } => Some(table),
            Symbol::Alias { .. } => None,
        }
    }

    /// 名字 → 列定义（穿透别名）。
    pub fn columns(&self, name: &str) -> Option<&[Column]> {
        Some(self.relation(name)?.columns())
    }

    /// `(关系, 列名)` → 列下标（穿透别名）。
    ///
    /// 将来的 binder 解列名用它；testbench 也用它。
    pub fn column_index(&self, relation: &str, column: &str) -> Option<usize> {
        self.columns(relation)?
            .iter()
            .position(|c| c.name == column)
    }

    /// 已经注册的名字（含别名），按插入顺序。
    pub fn names(&self) -> Vec<&str> {
        self.symbols.iter().map(|s| s.name()).collect()
    }

    // ------------------------------------------------------------ 索引（3.6）

    /// `CREATE INDEX [IF NOT EXISTS] name ON table (column)`。
    ///
    /// 立刻建好索引（代价这时候付），返回**生效的索引名**。三件事要如实报错：
    /// 表必须在（[`DbError::UnknownSymbol`]）、列必须在
    /// （[`DbError::ColumnOutOfBounds`]）、名字不能与任何表/别名/索引撞
    /// （[`DbError::DuplicateSymbol`]，`IF NOT EXISTS` 命中时视为满足要求）。
    ///
    /// > **与 PostgreSQL 的一处差异**：`IF NOT EXISTS` 命中时这里立刻返回、不碰
    /// > 表和列；但调用方（`irgen`）在调我们之前就已经把 `ON <表> (<列>)` 解出来了，
    /// > 所以写错表名/列名仍然会报错。PostgreSQL 是"命中就不看别的"——我们宁可把
    /// > 那个错字报出来（幂等脚本里的错字不该被吞掉）。
    pub fn create_index(
        &mut self,
        name: &str,
        table: &str,
        column: usize,
        if_not_exists: bool,
    ) -> Result<String> {
        let name = self.effective_name(name);
        let table = self.effective_name(table);

        let exists = self.indexes.iter().any(|index| index.name == name);
        if self.lookup(&name).is_some() || exists {
            if if_not_exists {
                return Ok(name);
            }
            return Err(DbError::DuplicateSymbol { name });
        }

        let relation = self
            .relation(&table)
            .ok_or_else(|| DbError::UnknownSymbol {
                name: table.clone(),
            })?;

        // 建一份索引：错的列号在这里就会被 `Index::build` 报出来
        let slot = IndexSlot::new(column);
        slot.build(relation)?;

        self.indexes.push(IndexSymbol {
            name: name.clone(),
            table,
            slot: Rc::new(slot),
        });
        Ok(name)
    }

    /// `DROP INDEX [IF EXISTS] name`。返回被删掉的索引名。
    pub fn drop_index(&mut self, name: &str, if_exists: bool) -> Result<String> {
        let name = self.effective_name(name);
        match self.indexes.iter().position(|index| index.name == name) {
            Some(at) => {
                self.indexes.remove(at);
                Ok(name)
            }
            None if if_exists => Ok(name),
            None => Err(DbError::UnknownSymbol { name }),
        }
    }

    /// 这张表某一列上的索引（可能没有）。优化器与执行器用它。
    pub fn index_of(&self, table: &str, column: usize) -> Option<Rc<IndexSlot>> {
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .find(|index| index.table == table && index.column() == column)
            .map(|index| Rc::clone(&index.slot))
    }

    /// 这张表上的索引：`(索引名, 列)`。`\d <表>` 用它。
    pub fn indexes_of(&self, table: &str) -> Vec<(String, usize)> {
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .filter(|index| index.table == table)
            .map(|index| (index.name.clone(), index.column()))
            .collect()
    }

    /// 目录里所有索引：`(索引名, 表名, 列)`（测试与显示用）。
    pub fn indexes(&self) -> Vec<(String, String, usize)> {
        self.indexes
            .iter()
            .map(|index| (index.name.clone(), index.table.clone(), index.column()))
            .collect()
    }

    /// 这张表上的索引要用的键：`(槽位, 这一行在那一列上的值)`。
    ///
    /// `INSERT` 在**把行交给表之前**算它（行要搬进表里，之后就拿不到了）；
    /// 没有索引的表返回空 `Vec`，于是装载几万行时这条路径是空转。
    pub fn index_keys_of(&self, table: &str, row: &Row) -> Vec<(Rc<IndexSlot>, Value)> {
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .filter(|index| index.table == table)
            .filter_map(|index| {
                row.get(index.column())
                    .map(|value| (Rc::clone(&index.slot), value.clone()))
            })
            .collect()
    }

    /// `INSERT` 之后把新行喂给索引：`rid` 是它落在表里的行号（就是插之前的行数）。
    ///
    /// 索引已经失效时是空转（下次用到时整体重建），所以这里**不会**扫表。
    pub fn maintain_indexes(&self, keys: Vec<(Rc<IndexSlot>, Value)>, rid: usize) {
        for (slot, key) in keys {
            slot.insert_row(key, rid);
        }
    }

    /// `UPDATE` 之后让这张表上的索引全部失效（下次用到时重建）。
    ///
    /// 为什么不是现场维护：一行的改动会挪动好几个键，逐个改与"整体作废、下次重建"
    /// 相比既容易错也不划算；`UPDATE` 本来就要扫全表。
    pub fn invalidate_indexes(&mut self, table: &str) {
        let table = self.effective_name(table);
        for index in &self.indexes {
            if index.table == table {
                index.slot.invalidate();
            }
        }
    }

    // ------------------------------------------------------ schema 与 search_path

    /// 登记一个 schema（`CREATE SCHEMA name`）。
    ///
    /// 重名时：写了 `IF NOT EXISTS` 就当作满足要求，否则报
    /// [`DbError::DuplicateSymbol`]。
    pub fn create_schema(&mut self, name: &str, if_not_exists: bool) -> Result<()> {
        if self.schemas.iter().any(|schema| schema == name) {
            return match if_not_exists {
                true => Ok(()),
                false => Err(DbError::DuplicateSymbol {
                    name: name.to_string(),
                }),
            };
        }
        self.schemas.push(name.to_string());
        Ok(())
    }

    /// 删掉一个 schema（`DROP SCHEMA [IF EXISTS] name [CASCADE]`）与它名下所有符号。
    ///
    /// 返回删掉的符号个数。写 `CASCADE` 时**不检查**依赖（本版没有引用检查）；
    /// 不写 `CASCADE` 而名下还有对象时明确报错——与 PostgreSQL 一样，
    /// 不能悄悄把别人的表删了。
    pub fn drop_schema(&mut self, name: &str, if_exists: bool, cascade: bool) -> Result<usize> {
        let prefix = format!("{}.", name);
        let doomed = self
            .symbols
            .iter()
            .filter(|symbol| symbol.name().starts_with(&prefix))
            .count();

        let doomed_indexes = self
            .indexes
            .iter()
            .filter(|index| {
                index.name.starts_with(&prefix) || index.table.starts_with(&prefix)
            })
            .count();

        if (doomed > 0 || doomed_indexes > 0) && !cascade {
            return Err(DbError::Unsupported {
                feature: format!(
                    "DROP SCHEMA {}（名下还有 {} 张表、{} 条索引，请写 CASCADE；本版不做依赖检查）",
                    name, doomed, doomed_indexes
                ),
            });
        }
        if doomed == 0
            && !if_exists
            && !self.schemas.iter().any(|schema| schema == name)
        {
            return Err(DbError::UnknownSymbol {
                name: name.to_string(),
            });
        }

        self.symbols
            .retain(|symbol| !symbol.name().starts_with(&prefix));
        self.indexes.retain(|index| {
            !index.name.starts_with(&prefix) && !index.table.starts_with(&prefix)
        });
        self.schemas.retain(|schema| schema != name);
        Ok(doomed)
    }

    /// 设置 `search_path`（`SET search_path TO …`）。
    ///
    /// 只记下来——它影响的是**以后**的建表与名字解析，不落盘、也不影响已经建好的
    /// 符号（它们的名字里已经带着 schema 了）。
    pub fn set_search_path(&mut self, path: Vec<String>) {
        self.search_path = path;
    }

    /// 当前 `search_path`。
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }

    /// 登记过的 schema 名。
    pub fn schemas(&self) -> &[String] {
        &self.schemas
    }

    /// 建表时用的名字：非限定名按 `search_path` 的第一个 schema 加前缀。
    ///
    /// 与 PostgreSQL 的差异：这里**不检查那个 schema 是否存在**（`CREATE SCHEMA`
    /// 只是记个名）。`search_path` 为空时原样返回，于是没设过它时的行为与过去一致。
    ///
    /// 它是**幂等**的（名字里已经有 `.` 就不再动），所以调用方可以先用它算出"这个
    /// 表会叫什么"，再去 [`SymbolTable::define`]——两条路得到同一个名字。
    pub fn effective_name(&self, name: &str) -> String {
        match (name.contains('.'), self.search_path.first()) {
            (false, Some(schema)) => format!("{}.{}", schema, name),
            _ => name.to_string(),
        }
    }

    /// 一个名字在当前 `search_path` 下要试的若干写法（按优先级）。
    ///
    /// 限定名（含 `.`）只有它自己；非限定名先按 `search_path` 顺序加前缀、最后
    /// 试裸名字。
    fn candidates(&self, name: &str) -> Vec<String> {
        if name.contains('.') {
            return vec![name.to_string()];
        }
        let mut out: Vec<String> = self
            .search_path
            .iter()
            .map(|schema| format!("{}.{}", schema, name))
            .collect();
        out.push(name.to_string());
        out
    }

    /// 名字 → 符号下标（认 `search_path`，不穿透别名）。
    fn symbol_index(&self, name: &str) -> Option<usize> {
        self.candidates(name)
            .iter()
            .find_map(|candidate| self.symbols.iter().position(|s| s.name() == candidate))
    }

    /// 顺着别名链找到最终的关系符号。
    ///
    /// 别名一定指向已存在的符号且名字不重复，所以链一定终止；这里的步数上限
    /// 只是防御性的，不依赖上面那个不变量。
    fn resolve(&self, name: &str) -> Option<&Symbol> {
        let index = self.resolve_index(name)?;
        self.symbols.get(index)
    }

    /// 同 [`resolve`](SymbolTable::resolve)，但返回下标而不是引用——
    /// 于是可以再拿它取可变借用。
    fn resolve_index(&self, name: &str) -> Option<usize> {
        let mut index = self.symbol_index(name)?;
        for _ in 0..=self.symbols.len() {
            match &self.symbols[index] {
                Symbol::Relation { .. } => return Some(index),
                Symbol::Alias { target, .. } => {
                    index = self.symbol_index(target)?;
                }
            }
        }
        None
    }
}
