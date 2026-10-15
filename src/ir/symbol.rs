//! 符号表:名字 → 符号的映射.
//!
//! 它是整个系统里"名字"的唯一权威.设计上要承接两件事:
//!
//! * **数据库加载时批量导入**:持久化层把表读回来之后,一次性
//!   [`SymbolTable::extend`] 进符号表,之后 `Scan` 就能按名字取到它.
//! * **`AS` 之类的构造往表里加东西**:给已有符号起个别名--
//!   这就是 [`SymbolTable::alias`],`Symbol::Alias` 是它的形状.
//!
//! 符号表**持有表实例**(`Box<dyn Table>`),而不只是形状,所以 `Scan` 按名字
//! 就能直接拿到数据;形状则通过 [`crate::db::table::TabShape`] 从表上取.
//!
//! # 索引也在这里(3.6)
//!
//! `CREATE INDEX` 建出来的索引是**目录对象**:[`IndexSymbol`] 记着名字,表与
//! 一个 [`IndexSlot`] ("列 + 当前那一份索引").读写路径都由它兜住--
//! `INSERT` 追加,`UPDATE` 作废,`DROP INDEX` 删掉,`DROP SCHEMA ... CASCADE`
//! 连同表一起清.索引**不落盘**.
//!
//! # 事务也在这里(4.1)
//!
//! 撤销记录(私有的 `Tx`)放在符号表里,因为**要撤销的东西全在这儿**:表数据(符号表
//! 持有 `Box<dyn Table>`),有哪些符号,索引,schema 名,`search_path`.两条口径:
//!
//! * **目录快照**在事务开始时取一份(只有名字,**不含数据**,所以是廉价的);
//! * **整表快照**按需--事务里**第一次**要改或被删某张表之前,把它整表克隆一份
//!   ([`Table::snapshot`]).于是大表事务的代价是"一次整表克隆",而不是"整个库".
//!
//! 两条不变式:**任何会改数据或删符号的路径都必须先 `snapshot_before_write`**
//! (`relation_mut` 与 `drop_schema` 是仅有的两处),否则回滚会静默丢表;
//! **回滚之后必须把所有索引槽作废**(行号可能变了,留着就是拿旧行号回答新键).

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::db::index::IndexSlot;
use crate::db::table::{Column, DbError, Result, Row, Table, Value};

/// 目录里的一条索引:`CREATE INDEX name ON table (column)`.
///
/// 名字与表名都是**生效名**(`search_path` 加过前缀的那种,与 `CREATE TABLE`
/// 同一套规则),所以查的时候要用 [`SymbolTable::effective_name`] 换算.
#[derive(Debug, Clone)]
pub struct IndexSymbol {
    /// 索引名(生效名).
    pub name: String,
    /// 它建在哪张表上(生效名).
    pub table: String,
    /// "列 + 当前那一份索引".执行器与优化器都借它.
    pub slot: Rc<IndexSlot>,
}

impl IndexSymbol {
    /// 索引建在哪一列.
    pub fn column(&self) -> usize {
        self.slot.column()
    }
}

/// 符号表里的一条记录.
pub enum Symbol {
    /// 一个关系:限定名 + 数据(数据同时提供形状).
    Relation {
        /// 限定名,例如 `study.people`.
        name: String,
        /// 绑定的表.
        table: Box<dyn Table>,
    },
    /// 别名:新名字指向一个**已经存在**的名字.
    ///
    /// 这就是将来 `AS` 的落点.
    Alias {
        /// 别名.
        name: String,
        /// 被指向的符号名.
        target: String,
    },
}

impl Symbol {
    /// 造一个关系符号.接受 `impl Table` 省掉调用方手写 `Box::new`.
    pub fn relation<T: Table + 'static>(name: impl Into<String>, table: T) -> Self {
        Symbol::Relation {
            name: name.into(),
            table: Box::new(table),
        }
    }

    /// 造一个别名符号.
    pub fn alias(alias: impl Into<String>, target: impl Into<String>) -> Self {
        Symbol::Alias {
            name: alias.into(),
            target: target.into(),
        }
    }

    /// 符号的名字.
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

/// 名字 → 符号的符号表.
///
/// 内部是一个插入有序的 `Vec`,线性查找.符号数量是个位数到几十,线性扫描比
/// 哈希表更快也更好读,而且顺序稳定,打印可预期.要换成 map 只是改这里.
///
/// # schema 与 `search_path`
///
/// 本版**没有完整的 schema 系统**:`study.movies` 这种限定名一直是被当作
/// **不透明的标识符**拼起来用的.这里补上的是两件配套的小事:
///
/// * `CREATE SCHEMA` 只是把名字**记下来**([`SymbolTable::create_schema`]);
/// * `SET search_path` 是真的生效的([`SymbolTable::set_search_path`]):
///   建表时非限定名按 `path[0]` 加前缀,解析名字时按 `path` 顺序试,
///   最后试裸名字.
///
/// 为什么后者不能只是"接受但无操作":`filmdb.sql` 的建表写的是非限定名,
/// 而测试查的是 `study.movies`--不作前缀那些表就永远不存在.
///
/// # 事务(4.1)
///
/// [`SymbolTable::begin`] / [`commit`](SymbolTable::commit) /
/// [`rollback`](SymbolTable::rollback) 是 `BEGIN` / `COMMIT` / `ROLLBACK` 的落点;
/// 引擎还会给**每条语句**开一个隐式事务(`begin_implicit`).
/// 撤销记录见文件头的"事务也在这里".
#[derive(Debug, Default)]
pub struct SymbolTable {
    symbols: Vec<Symbol>,
    /// `CREATE INDEX` 建出来的索引,按建表顺序.名字与表名都是生效名.
    ///
    /// **不落盘**:它是会话级的目录对象(重启就没了),见
    /// [`crate::db::index`].
    indexes: Vec<IndexSymbol>,
    /// `CREATE SCHEMA` 登记过的 schema 名(插入有序).
    schemas: Vec<String>,
    /// 当前 `search_path`;**空**表示不加前缀(默认,行为与过去完全一致).
    search_path: Vec<String>,
    /// **当前角色**:权限检查看的就是它([`crate::ir::stmt::Server`] 的口径).
    ///
    /// 它是**会话状态**(与 `search_path` 一样),不是目录:事务开始时随目录快照一起
    /// 记下,`ROLLBACK` 盖回去(`SET ROLE` 是能回滚的,PostgreSQL 也是).
    /// 打开库时由引擎设成**会话用户**(认证过的那个名字).
    role: String,
    /// 进行中的事务(没有就是 `None`).撤销记录在这里,见文件头.
    tx: Option<Tx>,
}

/// 事务的种类.
///
/// `Implicit` 是**引擎给每条语句**开的隐式事务(没有显式 `BEGIN` 时:成功即提交,
/// 失败即回滚,于是每条语句自己是原子的);`Explicit` 是用户 `BEGIN` 出来的,
/// 提交点从"这条语句末尾"挪到 `COMMIT`.
///
/// **隐式事务对用户是不可见的**:`BEGIN` 会把它升级成显式(否则那条 `BEGIN`
/// 会被隐式提交吞掉,后面的语句就不在事务里了),而对着一个隐式事务发
/// `COMMIT` / `ROLLBACK` 只当"没有事务"处理(照 PostgreSQL 的口径给一句警告).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxKind {
    /// 引擎给一条语句开的.
    Implicit,
    /// 用户 `BEGIN` 出来的.
    Explicit,
}

/// 一个进行中的事务的撤销记录.
struct Tx {
    kind: TxKind,
    /// 事务开始时的目录.只有名字,**不含数据**.
    catalog: CatalogSnapshot,
    /// 事务里被改过或被删掉的表:第一次碰它时整表克隆一份,回滚时盖回去.
    ///
    /// 键是表的**生效名**(符号表里存的那个),与脏表标记同一套口径.
    tables: BTreeMap<String, Box<dyn Table>>,
}

impl fmt::Debug for Tx {
    /// 手写 `Debug`:`Box<dyn Table>` 不要求 `Debug`,而 [`SymbolTable`] 派生着
    /// `Debug`.打印表名与行数就够定位问题了.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tx")
            .field("kind", &self.kind)
            .field(
                "tables",
                &self
                    .tables
                    .iter()
                    .map(|(name, table)| (name.as_str(), table.nrows()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// 事务开始时的目录:符号(只有名字),索引,schema 名,`search_path`,当前角色.
///
/// **数据不在里面**--它按需整表克隆(见 [`SymbolTable::snapshot_before_write`]).
struct CatalogSnapshot {
    symbols: Vec<SnapEntry>,
    indexes: Vec<IndexSymbol>,
    schemas: Vec<String>,
    search_path: Vec<String>,
    /// 当前角色(`SET ROLE` 也要能被 `ROLLBACK` 盖回去).
    role: String,
}

/// 目录快照里的一条符号.关系的**数据**不在快照里,只留名字.
#[derive(Debug)]
enum SnapEntry {
    /// 一个关系,名字是生效名.
    Relation(String),
    /// 一个别名.
    Alias {
        /// 别名.
        name: String,
        /// 被指向的符号名.
        target: String,
    },
}

impl SymbolTable {
    /// 空表.
    pub fn new() -> Self {
        SymbolTable {
            symbols: Vec::new(),
            indexes: Vec::new(),
            schemas: Vec::new(),
            search_path: Vec::new(),
            role: String::new(),
            tx: None,
        }
    }

    /// 注册一个关系.重名 → [`DbError::DuplicateSymbol`].
    ///
    /// 非限定名会按 `search_path` 的**第一个** schema 加前缀(见
    /// [`SymbolTable::effective_name`]).
    pub fn define<T: Table + 'static>(
        &mut self,
        name: impl Into<String>,
        table: T,
    ) -> Result<()> {
        let name = self.effective_name(&name.into());
        self.extend([Symbol::relation(name, table)])
    }

    /// 批量导入.
    ///
    /// 这就是"数据库被加载时初始化一批符号"的入口:持久化层读回一批表之后,
    /// 一次调用把它们全部装进符号表.整批是**原子的**--任何一条重名就整体失败,
    /// 不会留下装了一半的符号表.
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

    /// 给一个**已经存在**的名字起别名(`AS` 的落点).
    ///
    /// 目标必须已存在,且别名不能重名--这两条保证别名链天然无环,不需要环检测.
    pub fn alias(&mut self, alias: impl Into<String>, target: &str) -> Result<()> {
        if self.lookup(target).is_none() {
            return Err(DbError::UnknownSymbol {
                name: target.to_string(),
            });
        }
        self.extend([Symbol::alias(alias, target)])
    }

    /// 名字 → 符号(不穿透别名).
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.symbol_index(name).map(|index| &self.symbols[index])
    }

    /// 名字 → 关系(**穿透别名**).
    pub fn relation(&self, name: &str) -> Option<&dyn Table> {
        match self.resolve(name)? {
            Symbol::Relation { table, .. } => Some(&**table),
            // resolve 只会返回 Relation,这里只是让类型穷尽
            Symbol::Alias { .. } => None,
        }
    }

    /// 名字 → 关系,但**按存进来的名字精确取**:不认 `search_path`,不穿别名.
    ///
    /// 与 [`relation`](SymbolTable::relation) 的差别只在一处:名字**不**按当前
    /// `search_path` 展开.提交点(`Session::commit`)必须用这一个--脏表记的是
    /// **这张表自己的生效名**,若按当时的 `search_path` 再解一次,就可能解到另一张
    /// 同名的表上(`s.t` 与 `t` 都在,`search_path` 指向 `s`),于是**改了 A 却存了 B**,
    /// 真正的 A 悄悄地没落盘.
    pub fn relation_exact(&self, name: &str) -> Option<&dyn Table> {
        self.symbols.iter().find_map(|symbol| match symbol {
            Symbol::Relation { name: stored, table } if stored == name => Some(&**table),
            _ => None,
        })
    }

    /// 名字 → **可变**的表句柄(穿透别名).
    ///
    /// `INSERT` 要改表内容,而 [`relation`](SymbolTable::relation) 是只读的.
    /// 实现上分两步:先用只读借用解出下标,再按下标取 `&mut`--这也是为什么
    /// 别名穿透要有一个返回下标的版本.
    ///
    /// **这是数据唯一的写入口**,所以事务的整表快照也挂在这里:事务里第一次要写
    /// 这张表时先克隆一份(见私有的 `snapshot_before_write`).
    pub fn relation_mut(&mut self, name: &str) -> Option<&mut Box<dyn Table>> {
        let index = self.resolve_index(name)?;
        self.snapshot_before_write(index);
        match self.symbols.get_mut(index)? {
            Symbol::Relation { table, .. } => Some(table),
            Symbol::Alias { .. } => None,
        }
    }

    /// 名字 → 列定义(穿透别名).
    pub fn columns(&self, name: &str) -> Option<&[Column]> {
        Some(self.relation(name)?.columns())
    }

    /// `(关系, 列名)` → 列下标(穿透别名).
    ///
    /// 将来的 binder 解列名用它;testbench 也用它.
    pub fn column_index(&self, relation: &str, column: &str) -> Option<usize> {
        self.columns(relation)?
            .iter()
            .position(|c| c.name == column)
    }

    /// 已经注册的名字(含别名),按插入顺序.
    pub fn names(&self) -> Vec<&str> {
        self.symbols.iter().map(|s| s.name()).collect()
    }

    // ------------------------------------------------------------ 事务(4.1)

    /// 现在有事务吗(引擎开的隐式事务也算).
    pub fn in_transaction(&self) -> bool {
        self.tx.is_some()
    }

    /// 现在这个事务是哪种;没有事务就是 `None`.
    pub(crate) fn tx_kind(&self) -> Option<TxKind> {
        self.tx.as_ref().map(|tx| tx.kind)
    }

    /// `BEGIN`:开一个显式事务;当前是**隐式**事务时把它**升级**成显式的.
    ///
    /// 升级这一步不能少:引擎给每条语句都开了一个隐式事务,那条 `BEGIN` 自己也在
    /// 里面--不升级的话它会被自己的隐式事务提交吞掉,后面的语句根本不在事务里.
    ///
    /// 已经有一个**显式**事务时这里什么都不做(不报错也不嵌套):照 PostgreSQL 的
    /// 口径,调用方(`ir::stmt`)给一句警告就完事.
    pub fn begin(&mut self) {
        match self.tx.as_mut() {
            Some(tx) => tx.kind = TxKind::Explicit,
            None => {
                let catalog = self.snapshot_catalog();
                self.tx = Some(Tx {
                    kind: TxKind::Explicit,
                    catalog,
                    tables: BTreeMap::new(),
                });
            }
        }
    }

    /// 引擎给**一条语句**开的隐式事务.已经有事务(隐式或显式)时什么都不做.
    ///
    /// 只对写路径有意义:快照是**按需**的([`snapshot_before_write`]),所以一条
    /// 只读语句什么都没克隆,代价就是几个名字的拷贝.
    ///
    /// [`snapshot_before_write`]: SymbolTable::snapshot_before_write
    pub(crate) fn begin_implicit(&mut self) {
        if self.tx.is_none() {
            let catalog = self.snapshot_catalog();
            self.tx = Some(Tx {
                kind: TxKind::Implicit,
                catalog,
                tables: BTreeMap::new(),
            });
        }
    }

    /// `COMMIT`.返回**是否真的提交了一个显式事务**(`false` 表示该给一句警告).
    ///
    /// 隐式事务不归它管(那是引擎在语句末尾做的事),所以对着一个隐式事务发
    /// `COMMIT` 与"没有事务"是同一种情况--用户看不见隐式事务.
    pub fn commit(&mut self) -> bool {
        if self.tx_kind() != Some(TxKind::Explicit) {
            return false;
        }
        self.tx = None;
        true
    }

    /// `ROLLBACK`.返回**是否真的回滚了一个显式事务**(`false` 表示该给一句警告).
    ///
    /// 同 [`commit`](SymbolTable::commit):隐式事务与"没有事务"是同一种情况.
    pub fn rollback(&mut self) -> bool {
        if self.tx_kind() != Some(TxKind::Explicit) {
            return false;
        }
        self.undo();
        true
    }

    /// 隐式事务的提交(引擎在一条语句成功之后调):只有当前是隐式事务才关掉它.
    pub(crate) fn commit_implicit(&mut self) {
        if self.tx_kind() == Some(TxKind::Implicit) {
            self.tx = None;
        }
    }

    /// 隐式事务的回滚(引擎在一条语句失败之后调):只有当前是隐式事务才回滚.
    ///
    /// 显式事务里的语句失败**不**走这里--本版没有 `SAVEPOINT`,所以整句回滚要付
    /// 一次"每条语句都克隆一张表"的代价(`filmdb.sql` 那种 7.6 万条 INSERT 会变成
    /// O(n²)).要撤销就 `ROLLBACK` 整个事务.
    pub(crate) fn rollback_implicit(&mut self) {
        if self.tx_kind() == Some(TxKind::Implicit) {
            self.undo();
        }
    }

    /// 记下当前目录 -- 事务的开始点.
    fn snapshot_catalog(&self) -> CatalogSnapshot {
        let symbols = self
            .symbols
            .iter()
            .map(|symbol| match symbol {
                Symbol::Relation { name, .. } => SnapEntry::Relation(name.clone()),
                Symbol::Alias { name, target } => SnapEntry::Alias {
                    name: name.clone(),
                    target: target.clone(),
                },
            })
            .collect();

        CatalogSnapshot {
            symbols,
            indexes: self.indexes.clone(),
            schemas: self.schemas.clone(),
            search_path: self.search_path.clone(),
            role: self.role.clone(),
        }
    }

    /// 事务里**第一次**要改(或要删)第 `index` 个符号的表之前,整表克隆一份进 undo.
    ///
    /// 任何"会改数据 / 会删符号"的路径都必须先调它:`relation_mut`(`INSERT` 与
    /// `UPDATE` 唯一的写入口)与 `drop_schema`(符号连同数据一起消失).
    /// 键用**符号自己的名字**:回滚按目录快照重建,对的就是这个名字.
    fn snapshot_before_write(&mut self, index: usize) {
        let name = match self.symbols.get(index) {
            Some(Symbol::Relation { name, .. }) => name.clone(),
            // 别名:数据在它指向的关系符号上,那个符号自己会被钩子照到
            _ => return,
        };

        // 不在事务里,或者事务里已经克隆过它了:都不用再克隆.**已经克隆过的那一份
        // 才是要盖回去的**--再克隆一次就把它覆盖成"改过一半"的那份了.
        match self.tx.as_ref() {
            None => return,
            Some(tx) if tx.tables.contains_key(&name) => return,
            Some(_) => {}
        }

        let snapshot = match self.symbols.get(index) {
            Some(Symbol::Relation { table, .. }) => table.snapshot(),
            _ => return,
        };
        if let Some(tx) = self.tx.as_mut() {
            tx.tables.insert(name, snapshot);
        }
    }

    /// 把当前事务的改动全部撤销,并结束这个事务.
    ///
    /// 做法是"按目录快照重建":符号表整个按事务开始时的样子拼回来--改过 / 删过的
    /// 表用 undo 里的整表克隆,没动过的用现成的那一份;索引,schema 名,
    /// `search_path` 直接盖回去.
    fn undo(&mut self) {
        let Some(tx) = self.tx.take() else {
            return;
        };

        // 1) 把现成的关系收起来,重建时复用
        let mut live: BTreeMap<String, Box<dyn Table>> = BTreeMap::new();
        for symbol in std::mem::take(&mut self.symbols) {
            if let Symbol::Relation { name, table } = symbol {
                live.insert(name, table);
            }
        }

        // 2) 按快照重建符号表.事务开始时存在的关系,要么没动过(还在 `live` 里),
        //    要么被钩子克隆过(在 `tx.tables` 里);两边都没有只可能是某条删除路径
        //    漏了 `snapshot_before_write`--那会让这张表连同数据一起**静默消失**,
        //    所以这里宁可炸掉.
        let mut symbols = Vec::with_capacity(tx.catalog.symbols.len());
        let mut restored = tx.tables;
        for entry in tx.catalog.symbols {
            match entry {
                SnapEntry::Relation(name) => {
                    let table = restored
                        .remove(&name)
                        .or_else(|| live.remove(&name))
                        .unwrap_or_else(|| panic!("{}", tr!("ir.symbol.001", arg0 =  name)));
                    symbols.push(Symbol::Relation { name, table });
                }
                SnapEntry::Alias { name, target } => {
                    symbols.push(Symbol::Alias { name, target });
                }
            }
        }
        self.symbols = symbols;

        // 3) 目录的其余部分原样盖回去
        self.indexes = tx.catalog.indexes;
        self.schemas = tx.catalog.schemas;
        self.search_path = tx.catalog.search_path;
        self.role = tx.catalog.role;

        // 4) 索引是"键 → 行号":行号可能已经变了,一律作废,下次用到时重建.
        //    这一步不能省--留着就是拿旧行号回答新键,一个安静的错答案.
        for index in &self.indexes {
            index.slot.invalidate();
        }
    }

    // ------------------------------------------------------------ 索引(3.6)

    /// `CREATE INDEX [IF NOT EXISTS] name ON table (column)`.
    ///
    /// 立刻建好索引(代价这时候付),返回**生效的索引名**.三件事要如实报错:
    /// 表必须在([`DbError::UnknownSymbol`]),列必须在
    /// ([`DbError::ColumnOutOfBounds`]),名字不能与任何表/别名/索引撞
    /// ([`DbError::DuplicateSymbol`],`IF NOT EXISTS` 命中时视为满足要求).
    ///
    /// > **与 PostgreSQL 的一处差异**:`IF NOT EXISTS` 命中时这里立刻返回,不碰
    /// > 表和列;但调用方(`irgen`)在调我们之前就已经把 `ON <表> (<列>)` 解出来了,
    /// > 所以写错表名/列名仍然会报错.PostgreSQL 是"命中就不看别的"--我们宁可把
    /// > 那个错字报出来(幂等脚本里的错字不该被吞掉).
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

        // 建一份索引:错的列号在这里就会被 `Index::build` 报出来
        let slot = IndexSlot::new(column);
        slot.build(relation)?;

        self.indexes.push(IndexSymbol {
            name: name.clone(),
            table,
            slot: Rc::new(slot),
        });
        Ok(name)
    }

    /// `DROP INDEX [IF EXISTS] name`.返回被删掉的索引名.
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

    /// 这张表某一列上的索引(可能没有).优化器与执行器用它.
    pub fn index_of(&self, table: &str, column: usize) -> Option<Rc<IndexSlot>> {
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .find(|index| index.table == table && index.column() == column)
            .map(|index| Rc::clone(&index.slot))
    }

    /// 这张表上的索引:`(索引名, 列)`.`\d <表>` 用它.
    pub fn indexes_of(&self, table: &str) -> Vec<(String, usize)> {
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .filter(|index| index.table == table)
            .map(|index| (index.name.clone(), index.column()))
            .collect()
    }

    /// 目录里所有索引:`(索引名, 表名, 列)`(测试与显示用).
    pub fn indexes(&self) -> Vec<(String, String, usize)> {
        self.indexes
            .iter()
            .map(|index| (index.name.clone(), index.table.clone(), index.column()))
            .collect()
    }

    /// 这张表上的索引要用的键:`(槽位, 这一行在那一列上的值)`.
    ///
    /// `INSERT` 在**把行交给表之前**算它(行要搬进表里,之后就拿不到了);
    /// 没有索引的表返回空 `Vec`,于是装载几万行时这条路径是空转.
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

    /// `INSERT` 之后把新行喂给索引:`rid` 是它落在表里的行号(就是插之前的行数).
    ///
    /// 索引已经失效时是空转(下次用到时整体重建),所以这里**不会**扫表.
    pub fn maintain_indexes(&self, keys: Vec<(Rc<IndexSlot>, Value)>, rid: usize) {
        for (slot, key) in keys {
            slot.insert_row(key, rid);
        }
    }

    /// `UPDATE` 之后让这张表上的索引全部失效(下次用到时重建).
    ///
    /// 为什么不是现场维护:一行的改动会挪动好几个键,逐个改与"整体作废,下次重建"
    /// 相比既容易错也不划算;`UPDATE` 本来就要扫全表.
    pub fn invalidate_indexes(&mut self, table: &str) {
        let table = self.effective_name(table);
        for index in &self.indexes {
            if index.table == table {
                index.slot.invalidate();
            }
        }
    }

    // ------------------------------------------------------ schema 与 search_path

    /// 登记一个 schema(`CREATE SCHEMA name`).
    ///
    /// 重名时:写了 `IF NOT EXISTS` 就当作满足要求,否则报
    /// [`DbError::DuplicateSymbol`].
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

    /// 删掉一个 schema(`DROP SCHEMA [IF EXISTS] name [CASCADE]`)与它名下所有符号.
    ///
    /// 返回删掉的符号个数.写 `CASCADE` 时**不检查**依赖(本版没有引用检查);
    /// 不写 `CASCADE` 而名下还有对象时明确报错--与 PostgreSQL 一样,
    /// 不能悄悄把别人的表删了.
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
                feature: format!("{}", tr!("ir.symbol.002", arg0 = 
                    name, arg1 =  doomed, arg2 =  doomed_indexes
                )),
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

        // 要删掉的表先把**数据**克隆进 undo:`retain` 一走,这些表的内存里就再也
        // 没有第二份了,只回滚目录(留下一个空壳名字)比不实现事务更糟.
        let doomed_slots: Vec<usize> = self
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| symbol.name().starts_with(&prefix))
            .map(|(index, _)| index)
            .collect();
        for index in doomed_slots {
            self.snapshot_before_write(index);
        }

        self.symbols
            .retain(|symbol| !symbol.name().starts_with(&prefix));
        self.indexes.retain(|index| {
            !index.name.starts_with(&prefix) && !index.table.starts_with(&prefix)
        });
        self.schemas.retain(|schema| schema != name);
        Ok(doomed)
    }

    /// 设置 `search_path`(`SET search_path TO ...`).
    ///
    /// 只记下来--它影响的是**以后**的建表与名字解析,不落盘,也不影响已经建好的
    /// 符号(它们的名字里已经带着 schema 了).
    pub fn set_search_path(&mut self, path: Vec<String>) {
        self.search_path = path;
    }

    /// 当前 `search_path`.
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }

    /// 当前角色(`SET ROLE` 之后就是它).
    ///
    /// 权限检查拿它去问 [`crate::ir::stmt::Server`];打开库时是**会话用户**,
    /// 内存模式下是空串(那时没有任何检查).
    pub fn role(&self) -> &str {
        &self.role
    }

    /// 换当前角色(`SET ROLE` / `RESET ROLE`).
    ///
    /// 它随事务的目录快照一起被记下,所以 `ROLLBACK` 会盖回去--与 `search_path`
    /// 同一套.(校验"能不能借这个身份"不在这里,那是 [`crate::ir::stmt::Server`] 的事.)
    pub fn set_role(&mut self, role: impl Into<String>) {
        self.role = role.into();
    }

    /// 登记过的 schema 名.
    pub fn schemas(&self) -> &[String] {
        &self.schemas
    }

    /// 建表时用的名字:非限定名按 `search_path` 的第一个 schema 加前缀.
    ///
    /// 与 PostgreSQL 的差异:这里**不检查那个 schema 是否存在**(`CREATE SCHEMA`
    /// 只是记个名).`search_path` 为空时原样返回,于是没设过它时的行为与过去一致.
    ///
    /// 它是**幂等**的(名字里已经有 `.` 就不再动),所以调用方可以先用它算出"这个
    /// 表会叫什么",再去 [`SymbolTable::define`]--两条路得到同一个名字.
    pub fn effective_name(&self, name: &str) -> String {
        match (name.contains('.'), self.search_path.first()) {
            (false, Some(schema)) => format!("{}.{}", schema, name),
            _ => name.to_string(),
        }
    }

    /// 一个名字在当前 `search_path` 下要试的若干写法(按优先级).
    ///
    /// 限定名(含 `.`)只有它自己;非限定名先按 `search_path` 顺序加前缀,最后
    /// 试裸名字.
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

    /// 名字 → 符号下标(认 `search_path`,不穿透别名).
    fn symbol_index(&self, name: &str) -> Option<usize> {
        self.candidates(name)
            .iter()
            .find_map(|candidate| self.symbols.iter().position(|s| s.name() == candidate))
    }

    /// 顺着别名链找到最终的关系符号.
    ///
    /// 别名一定指向已存在的符号且名字不重复,所以链一定终止;这里的步数上限
    /// 只是防御性的,不依赖上面那个不变量.
    fn resolve(&self, name: &str) -> Option<&Symbol> {
        let index = self.resolve_index(name)?;
        self.symbols.get(index)
    }

    /// 同 [`resolve`](SymbolTable::resolve),但返回下标而不是引用--
    /// 于是可以再拿它取可变借用.
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
