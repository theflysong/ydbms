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
//! 持有 `Box<dyn Table>`),有哪些符号,索引,schema 名,`search_path`.四条口径:
//!
//! * **目录快照**在事务开始时取一份(只有名字,**不含数据**,所以是廉价的);
//! * **表的撤销记录按需**,而且按**写的方式**分两种(TODO 3.17 与 7.6):
//!   [`relation_append`](SymbolTable::relation_append)(`INSERT` / `COPY`)只记下
//!   "第一次追加之前的行数"--撤销是截断,表里的值一个都不复制;
//!   [`update_row`](SymbolTable::update_row)(`UPDATE`)按**行**记:改哪一行就存哪
//!   一行改之前的样子(整表克隆的年代改 1 行也要复制整张表,500k 行的表上是
//!   230 ms);`drop_schema` 更省--它把整张表**搬**进撤销记录,一个值都不复制.
//!   两种按行的记录可以叠在同一张表上,撤销时"先写回旧值,再截断追加";
//! * **任何会改数据或删符号的路径都必须先记一笔**(`relation_append` /
//!   `update_row` / `drop_schema` 是仅有的三处),否则回滚会静默丢表;
//! * **回滚之后必须把所有索引槽作废**(行号可能变了,留着就是拿旧行号回答新键).

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::db::index::IndexSlot;
use crate::db::phys::PhysTab;
use crate::db::table::{Column, ColumnsShape, DbError, Result, Row, Table, Value};

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
    /// 它是不是 `PRIMARY KEY` 那条约束**顺手**建出来的(TODO 8.7).
    ///
    /// 只用来拒绝 `DROP INDEX`:隐含索引与约束是一体的,而本版没有
    /// `ALTER TABLE ... DROP CONSTRAINT`,放行就等于"约束说没就没".显式的
    /// `CREATE INDEX` / `CREATE UNIQUE INDEX` 一律为 `false`.
    pub implicit: bool,
}

impl IndexSymbol {
    /// 索引建在哪一列.
    pub fn column(&self) -> usize {
        self.slot.column()
    }

    /// 这份索引强制唯一性吗.
    pub fn unique(&self) -> bool {
        self.slot.unique()
    }
}

/// "这一列上本该有一条索引"(TODO 8.7 的口径).
///
/// 它把 `PRIMARY KEY` / **单列** `UNIQUE` 与运行时那两种索引接上:按 PostgreSQL
/// 的口径合成名字,并决定这一份强不强制唯一性.
///
/// * **主键**:`<表>_pkey`,**强制**唯一性(重复键报错);
/// * **`UNIQUE` 列**:`<表>_<列名>_key`,只当**访问路径**,重复键照旧放行
///   (与主键有意不同,口径见 `src/ir/AGENTS.md`).
///
/// `table` 必须是**生效名**(`imdb.title_crew`),合成出来的名字也带那一段前缀:
/// 索引与它的表在同一个 schema 里,这是 PostgreSQL 的口径.这一点不是风格--
/// **打开库时 `rebuild_indexes` 跑在 `set_search_path` 之前**(库级默认值还没套上
/// 会话),按 `search_path` 加前缀的话,同一张表的索引在"刚建出来"与"重新打开"
/// 两条路上会得到**两个不同的名字**,而选择路径是按列查的,于是重开之后索引就
/// **看不见了**(实测:这个坑让 `Test 10.3` 从"跑得完"变回"跑不完").
///
/// 返回 `(合成名, 强不强制唯一性)`,给 [`SymbolTable::ensure_index`] 用.
pub fn implicit_index(
    table: &str,
    columns: &[Column],
    column: usize,
    primary: bool,
) -> (String, bool) {
    let name = match (primary, columns.get(column)) {
        (true, _) => format!("{}_pkey", table),
        (false, Some(column_def)) => format!("{}_{}_key", table, column_def.name),
        // 列号越界:名字随便给一个,`Index::build` 会把它报出来
        (false, None) => format!("{}_key", table),
    };
    (name, primary)
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
    /// 事务里被改过或被删掉的表:第一次碰它时记一条撤销记录,回滚时用它盖回去.
    ///
    /// 键是表的**生效名**(符号表里存的那个),与脏表标记同一套口径.
    tables: BTreeMap<String, UndoTable>,
}

/// 一张表的撤销记录.
///
/// 三种形态,同一张表可能**叠**着好几种:
///
/// * **纯追加**(`INSERT` / `COPY`):只记下"事务里第一次追加之前的行数",
///   撤销 = 截断回去--表里的值一个都不用复制(TODO 3.17);
/// * **按行改写**(`UPDATE`,TODO 7.6):改哪一行就存哪一行**改之前**的样子,
///   撤销 = 逐行写回.同一个行号只记第一次(要盖回去的是最早那份);
/// * **整张表搬走**(`DROP SCHEMA ... CASCADE`):表是**移**进这里来的,不是克隆
///   --回滚时移回去,一个值都不复制.
///
/// 先追加后改写(`BEGIN; INSERT; UPDATE; ROLLBACK;`)时前两种会叠在一起:撤销的
/// 顺序是"先写回旧值,再截断到追加前的行数"--被追加的那些行本来就在截断的范围里,
/// 写回它们再截掉,一步都不多余.
#[derive(Default)]
struct UndoTable {
    /// 纯追加:事务里**第一次**追加之前的行数.
    appended: Option<usize>,
    /// 原地改写:行号 → 改之前的那一行.
    rows: BTreeMap<usize, Row>,
    /// 整张表(连带它的形状与装饰)从符号表里搬出来的那一份.
    owned: Option<Box<dyn Table>>,
}

impl fmt::Debug for Tx {
    /// 手写 `Debug`:`Box<dyn Table>` 不要求 `Debug`,而 [`SymbolTable`] 派生着
    /// `Debug`.打印表名与"这份撤销记录是什么形态"就够定位问题了.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tables: Vec<(&str, String)> = self
            .tables
            .iter()
            .map(|(name, undo)| {
                // 名字是给读日志的人看的:用 `rows` / `truncate` / `owned` 这几个
                // 标识符,不要写文案(文案一律走 `tr!`,见 `src/i18n.rs`)
                let shape = match &undo.owned {
                    Some(table) => format!("owned({})", table.nrows()),
                    None if !undo.rows.is_empty() => {
                        format!("rows({})", undo.rows.len())
                    }
                    None => format!("truncate({})", undo.appended.unwrap_or(0)),
                };
                (name.as_str(), shape)
            })
            .collect();
        f.debug_struct("Tx")
            .field("kind", &self.kind)
            .field("tables", &tables)
            .finish()
    }
}

/// 事务开始时的目录:符号(只有名字),索引,schema 名,`search_path`,当前角色.
///
/// **数据不在里面**--它按需记一笔(改过的那几行,或者"截断回 N 行");见
/// [`SymbolTable::note_append`] 与 [`SymbolTable::update_row`].
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

    /// 名字 → **可变**的表句柄,并且按**纯追加**记撤销记录(`INSERT` / `COPY` 用).
    ///
    /// 追加只往表尾加行,所以撤销只需要"把表截断回原来的 N 行"(TODO 3.17)--
    /// 一个值都不复制,大表也一样便宜.
    ///
    /// `UPDATE` **不走这里**:它改的是已有行的内容,那种撤销记录得**按行**记
    /// ([`update_row`](SymbolTable::update_row),TODO 7.6).
    pub fn relation_append(&mut self, name: &str) -> Option<&mut Box<dyn Table>> {
        let index = self.resolve_index(name)?;
        self.note_append(index);
        self.write_handle(index)
    }

    /// 改第 `r` 行 -- `UPDATE` 的落库口,而且**顺带把这一行的旧值记进撤销记录**
    /// (TODO 7.6).
    ///
    /// 老做法在第一次写某张表时克隆整张表(`Table::snapshot`):改 1 行也要复制
    /// 全部数据,50 万行的表上是 230 ms,而且改的行越少越亏.现在按行记:改哪几行
    /// 就留哪几行的旧值,`ROLLBACK` 逐行写回.
    ///
    /// **新行由 `f` 从旧行算出来**:新行要在**改之前**的那一行上求值(`SET a = a + 1`
    /// 里的 `a` 是老值),而旧值正是这里要记下来的东西--所以让调用方拿着它算,
    /// 一次读取就够(各读一遍就是两倍的整行克隆,而这一条是逐行跑的热路径).
    /// 旧值**先记后写**:`f` 报错时这一行还没被动过,记录里多一份原样的值无害.
    ///
    /// **同一个行号只记第一次**:要盖回去的是这一行最早的样子的.
    ///
    /// 没在事务里(手写调用)就不记--那时也没有回滚这回事.
    pub fn update_row<F>(&mut self, table: &str, r: usize, f: F) -> Result<()>
    where
        F: FnOnce(&Row) -> Result<Row>,
    {
        let index = self
            .resolve_index(table)
            .ok_or_else(|| DbError::UnknownSymbol {
                name: table.to_string(),
            })?;
        let name = match self.symbols.get(index) {
            Some(Symbol::Relation { name, .. }) => name.clone(),
            // `resolve_index` 只给关系符号,这里到不了
            _ => return Err(DbError::UnknownSymbol { name: table.to_string() }),
        };

        // 旧值(**改之前**的那一行):表上读一次,读不到就是行号越界
        let old = match self.symbols.get(index) {
            Some(Symbol::Relation { table, .. }) => table.row(r),
            _ => None,
        }
        .ok_or(DbError::RowOutOfBounds {
            index: r,
            nrows: self.symbols.get(index).map_or(0, |symbol| match symbol {
                Symbol::Relation { table, .. } => table.nrows(),
                Symbol::Alias { .. } => 0,
            }),
        })?;

        // 新行在旧行上求值(调用方拿的是借来的旧值,不必再克隆一遍)
        let new_row = f(&old)?;

        if let Some(tx) = self.tx.as_mut() {
            tx.tables
                .entry(name)
                .or_default()
                .rows
                .entry(r)
                .or_insert(old);
        }

        self.write_handle(index)
            .ok_or_else(|| DbError::UnknownSymbol {
                name: table.to_string(),
            })?
            .update_row(r, new_row)
    }

    /// 按符号下标取可变句柄(几个写入口共用).
    fn write_handle(&mut self, index: usize) -> Option<&mut Box<dyn Table>> {
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
    /// 只对写路径有意义:撤销记录是**按需**的([`snapshot_before_write`]),所以一条
    /// 只读语句什么都没记,代价就是几个名字的拷贝.
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

    /// 事务里**第一次**要往第 `index` 个符号的表尾追加之前,记一条撤销记录.
    ///
    /// `relation_append`(`INSERT` / `COPY`)走它:记的只是"第一次追加之前的行数",
    /// 撤销 = 截断回去(见 [`UndoTable::appended`]).第二次追加不必再记--那个行数
    /// 还是同一个.键用**符号自己的名字**:回滚按目录快照重建,对的就是这个名字.
    ///
    /// 别的写路径不从这里走:`UPDATE` 按行记
    /// ([`update_row`](SymbolTable::update_row)),删表把整张表搬走
    /// (`drop_schema`,见 [`SymbolTable::take_for_undo`]).
    fn note_append(&mut self, index: usize) {
        let name = match self.symbols.get(index) {
            Some(Symbol::Relation { name, .. }) => name.clone(),
            // 别名:数据在它指向的关系符号上,那个符号自己会被钩子照到
            _ => return,
        };
        let appended = match self.symbols.get(index) {
            Some(Symbol::Relation { table, .. }) => table.nrows(),
            _ => return,
        };

        if let Some(tx) = self.tx.as_mut() {
            let undo = tx.tables.entry(name).or_default();
            if undo.appended.is_none() {
                undo.appended = Some(appended);
            }
        }
    }

    /// `DROP SCHEMA ... CASCADE` 的撤销记录:把第 `index` 个符号的表**搬**进
    /// 事务(不是克隆).
    ///
    /// 表马上要从符号表里 `retain` 掉了,那之后内存里就只剩这一份.老做法在这里
    /// 克隆整张表(`Table::snapshot`),于是"删一个 schema"要先复制它下面所有的
    /// 数据;搬走一个值都不复制,回滚时再搬回来.
    fn take_for_undo(&mut self, index: usize) {
        let Some(tx) = self.tx.as_mut() else {
            return;
        };
        let Some(Symbol::Relation { name, table }) = self.symbols.get_mut(index) else {
            return;
        };

        let undo = tx.tables.entry(name.clone()).or_default();
        // 已经搬过就不再搬第二回(同一张表在一次事务里被删两次不可能,但这里要幂等)
        if undo.owned.is_some() {
            return;
        }
        // 形状照旧的那张空表顶上:符号马上要被 `retain` 掉,这个占位没人看得见.
        // 形状要从被搬走的那张表上取,所以先借它的列定义.
        let shape = ColumnsShape::new(table.columns().to_vec());
        let name = table.name().to_string();
        let placeholder = PhysTab::new(&name, &shape);
        undo.owned = Some(std::mem::replace(table, Box::new(placeholder)));
    }

    /// 把当前事务的改动全部撤销,并结束这个事务.
    ///
    /// 做法是"按目录快照重建":符号表整个按事务开始时的样子拼回来--改过的表按
    /// 撤销记录逐行写回,被删掉的表**搬**回来,没动过的用现成的那一份;索引,
    /// schema 名,`search_path` 直接盖回去.
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
        //    要么被钩子记过(在 `tx.tables` 里);两边都没有只可能是某条删除路径
        //    漏了记一笔--那会让这张表连同数据一起**静默消失**,所以这里宁可炸掉.
        let mut symbols = Vec::with_capacity(tx.catalog.symbols.len());
        let mut restored = tx.tables;
        for entry in tx.catalog.symbols {
            match entry {
                SnapEntry::Relation(name) => {
                    let mut undo = restored.remove(&name);

                    // 表的数据在哪儿:被 `DROP SCHEMA ... CASCADE` **搬**进撤销记录
                    // 了(`owned`),还是仍在 `live` 里(搬走时留下的空壳不再有人要)
                    let owned = undo.as_mut().and_then(|undo| undo.owned.take());
                    let mut table = match owned {
                        Some(table) => table,
                        None => live.remove(&name).unwrap_or_else(|| {
                            panic!("{}", tr!("ir.symbol.001", arg0 = name))
                        }),
                    };

                    // 先按行写回旧值,再截断掉追加的那些行--两种记录可以叠在同一张
                    // 表上(`BEGIN; INSERT; UPDATE; ROLLBACK;`),而被追加的行本来
                    // 就在截断的范围里.
                    if let Some(undo) = undo {
                        for (r, old) in undo.rows {
                            // 写不回去就没有第二份数据可以拿出来盖:这是**实现坏了**
                            // (能 `update_row` 的表必须收得下它自己原来的那一行),
                            // 宁可炸掉也不留给用户一张没回滚掉的表.
                            if let Err(error) = table.update_row(r, old) {
                                panic!("{}", tr!("ir.symbol.005", arg0 = name, arg1 = r, arg2 = error));
                            }
                        }
                        // 追加式的撤销记录:截断回事务里第一次追加之前的行数
                        // (表里的值一个都不用复制,TODO 3.17)
                        if let Some(rows) = undo.appended {
                            if let Err(error) = table.truncate_to(rows) {
                                // 截断不了就没有第二份数据可以拿出来盖:这是**实现坏了**
                                // (能 `insert` 的表必须覆盖 `truncate_to`),宁可炸掉也
                                // 不留给用户一张没回滚掉的表.
                                panic!("{}", tr!("ir.symbol.004", arg0 = name, arg1 = error));
                            }
                        }
                    }

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
        unique: bool,
        if_not_exists: bool,
    ) -> Result<String> {
        let name = self.effective_name(name);
        let table = self.effective_name(table);

        let exists = self.indexes.iter().any(|index| index.name == name);
        if self.lookup(&name).is_some() || exists {
            if if_not_exists {
                // PostgreSQL 的 `IF NOT EXISTS` 就是看**名字**:名字在,它什么都不做
                // (哪怕那条索引建在别的表/别的列上).保留这个口径,免得幂等脚本
                // 里的 `CREATE INDEX IF NOT EXISTS` 突然开始报错.
                return Ok(name);
            }
            return Err(DbError::DuplicateSymbol { name });
        }

        let (name, _) = self.ensure_index(&name, &table, column, unique, if_not_exists, false)?;
        Ok(name)
    }

    /// "这一列上要有一条索引"的唯一落点(显式的 `CREATE INDEX` 与主键那条隐含索引
    /// 都走它).
    ///
    /// * `name` **空** 表示没有名字可依:按 PostgreSQL 的口径合成
    ///   ([`implicit_index`]),撞名就加 `_1`,`_2` 直到空出来;
    /// * `name` 非空就是显式名字,冲突的处理与 [`create_index`](SymbolTable::create_index)
    ///   原样一致(报 [`DbError::DuplicateSymbol`],`if_not_exists` 命中时放行);
    /// * `unique` 决定这一份强不强制唯一性--**主键**与 `CREATE UNIQUE INDEX` 是
    ///   `true`,单列 `UNIQUE` 那份是 `false`(见 [`crate::db::index`]);
    /// * `implicit` 记的是"它是约束顺手建出来的":只影响
    ///   [`drop_index`](SymbolTable::drop_index) 的口径.
    ///
    /// 返回 `(生效索引名, 这一次是不是真建了)`:已经有一份**同列且唯一性一致**的
    /// 索引时复用它,返回 `false`--于是
    /// [`rebuild_indexes`](SymbolTable::rebuild_indexes) 是幂等的,重复调用与"用户
    /// 自己 `CREATE INDEX` 过的那一列"都不会再建第二份.
    pub fn ensure_index(
        &mut self,
        name: &str,
        table: &str,
        column: usize,
        unique: bool,
        if_not_exists: bool,
        implicit: bool,
    ) -> Result<(String, bool)> {
        let table = self.effective_name(table);
        // **裸名**在这里展开成生效名(与表名同一套规则).显式 (`CREATE INDEX`) 走
        // 那条路时 `create_index` 已经展开过一次,这里再展开是**幂等**的;隐含的
        // (`PRIMARY KEY` 顺手那条)给的是裸名,就靠这一句落到表所在的 schema 里.
        let name = self.effective_name(name);

        // 同一个名字已经有一条索引:要么报错,要么(写了 `IF NOT EXISTS`)当满足要求
        if let Some(at) = self.indexes.iter().position(|index| index.name == name) {
            if !if_not_exists {
                return Err(DbError::DuplicateSymbol {
                    name: name.to_string(),
                });
            }
            return Ok((self.indexes[at].name.clone(), false));
        }
        // 名字被**表 / 别名**占了(它们在同一个名单里).合成名可以直接换一个;
        // 显式名字只能报错,与 `create_index` 的老口径一致.
        if !name.is_empty() && self.lookup(&name).is_some() {
            if if_not_exists {
                return Ok((name.to_string(), false));
            }
            return Err(DbError::DuplicateSymbol {
                name: name.to_string(),
            });
        }

        // 这一列上已经有一份**口径相同**的索引,就用它:同一个"列 + 唯一性"建两份
        // 是纯浪费,而且会让 `indexes_of` 报出两条一样的.
        if let Some(index) = self.indexes.iter().find(|index| {
            index.table == table && index.column() == column && index.unique() == unique
        }) {
            return Ok((index.name.clone(), false));
        }

        let name = match name.is_empty() {
            true => self.auto_index_name("", &table, column, unique),
            false => name,
        };

        let relation = self.relation(&table).ok_or_else(|| DbError::UnknownSymbol {
            name: table.clone(),
        })?;

        // 建一份索引:错的列号在这里就会被 `Index::build` 报出来;`unique` 的还会
        // 顺带查重(表里已经有重复行就建不出来).
        let slot = IndexSlot::new(column, unique, name.clone());
        slot.build(relation)?;

        self.indexes.push(IndexSymbol {
            name: name.clone(),
            table,
            slot: Rc::new(slot),
            implicit,
        });
        Ok((name, true))
    }

    /// 合成一个**没被占用**的索引名(占用 = 任何符号名,或任何索引名).
    ///
    /// `desired` 是 [`implicit_index`] 给的口径名(空串表示"没有名字可依",由这里
    /// 现算),撞上了就加 `_1`,`_2`,......索引的选择路径按**列**查,名字只用于显示
    /// 与报错,所以这里怎么退让都不会影响哪条查询走索引.
    fn auto_index_name(&self, desired: &str, table: &str, column: usize, unique: bool) -> String {
        let base = match desired.is_empty() {
            true => implicit_index(table, self.columns(table).unwrap_or(&[]), column, unique).0,
            false => desired.to_string(),
        };
        if !self.name_taken(&base) {
            return base;
        }
        for n in 1.. {
            let candidate = format!("{}_{}", base, n);
            if !self.name_taken(&candidate) {
                return candidate;
            }
        }
        unreachable!("a free index name always exists")
    }

    /// 这个名字被符号或索引占了吗.
    fn name_taken(&self, name: &str) -> bool {
        self.lookup(name).is_some() || self.indexes.iter().any(|index| index.name == name)
    }

    /// 建表时给 `PRIMARY KEY` / 单列 `UNIQUE` 顺手建的那条索引(TODO 8.7).
    ///
    /// `enforced` 决定这一份强不强制唯一性(主键强制,`UNIQUE` 不强制),也决定
    /// 合成名的后缀(`_pkey` / `_<列>_key`).返回 `(索引名, 这一次是不是真建了)`.
    ///
    /// 合成名是**裸名**([`implicit_index`]),这里交给 `effective_name` 按当时的
    /// `search_path` 加前缀:于是 `search_path` 是 `imdb` 时它叫 `imdb.movies_pkey`,
    /// 与 PostgreSQL 把索引放进表所在的 schema 同一套,而且一张表只有一个索引名.
    pub fn ensure_implicit_index(
        &mut self,
        table: &str,
        column: usize,
        enforced: bool,
    ) -> Result<(String, bool)> {
        let table = self.effective_name(table);
        // 合成名已经带**表所在的 schema**(见 [`implicit_index`] 的"打开库"那段),
        // 而 `ensure_index` 那次 `effective_name` 是幂等的,不会再动它.
        let (desired, _) = {
            let columns = self.columns(&table).unwrap_or(&[]);
            implicit_index(&table, columns, column, enforced)
        };
        // `if_not_exists = true`:同名索引已经在那儿(比如 `rebuild_indexes` 第二次
        // 跑)就复用它--这条路上"名字撞了"永远不该是错误,它就是我刚建的那一条.
        self.ensure_index(&desired, &table, column, enforced, true, true)
    }

    /// 建表时该给哪几列顺手建索引:`(表名, 列下标, 强不强制唯一性)`.
    ///
    /// 判据只有一处([`crate::db::table::Unique`]),于是"哪一列上有隐含索引"
    /// 不会有两份说法.
    pub fn unique_columns(&self) -> Vec<(String, usize, bool)> {
        let mut out = Vec::new();
        for symbol in &self.symbols {
            let Symbol::Relation { name, table } = symbol else {
                continue;
            };
            for (column, def) in table.columns().iter().enumerate() {
                if !def.unique.is_none() {
                    out.push((name.clone(), column, def.unique.enforced()));
                }
            }
        }
        out
    }

    /// 打开库之后,把 `Column::unique` 标记的那些索引**重建**回来.
    ///
    /// 索引是会话级的目录对象(不落盘),而"这一列上本该有访问路径"是**表的事实**
    /// (跟着模式文件走).没有这一步,`ysql` 一重启主键就退回全表扫--`tests.sql`
    /// 的 Test 10.3 会当场从"跑得完"变成"跑不完".
    ///
    /// 幂等:已经有**同列同口径**索引的列跳过(见 [`ensure_index`](SymbolTable::ensure_index)),
    /// 所以重复调用,以及"用户自己 `CREATE INDEX` 过的那一列"都不会建第二份.
    /// 返回这一次真建了哪几条(给测试与报告用).
    pub fn rebuild_indexes(&mut self) -> Result<Vec<(String, usize)>> {
        let mut built = Vec::new();
        for (table, column, enforced) in self.unique_columns() {
            let (name, created) = self.ensure_implicit_index(&table, column, enforced)?;
            if created {
                built.push((name, column));
            }
        }
        Ok(built)
    }

    /// `DROP INDEX [IF EXISTS] name`.返回被删掉的索引名.
    ///
    /// **隐含索引(主键那条)不许单独删**:它与约束是一体的,而本版没有
    /// `ALTER TABLE ... DROP CONSTRAINT`,删掉就等于"约束说没就没".显式建的索引
    /// 照删不误.
    pub fn drop_index(&mut self, name: &str, if_exists: bool) -> Result<String> {
        let name = self.effective_name(name);
        match self.indexes.iter().position(|index| index.name == name) {
            Some(at) if self.indexes[at].implicit => Err(DbError::Unsupported {
                feature: format!("{}", tr!("ir.symbol.003", arg0 = name)),
            }),
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

    /// 表名是**生效名**:哪些索引强不强制唯一性(TODO 8.7).
    pub fn unique_slots_of(&self, table: &str) -> Vec<(Rc<IndexSlot>, String)> {
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .filter(|index| index.table == table && index.unique())
            .map(|index| (Rc::clone(&index.slot), index.name.clone()))
            .collect()
    }

    /// 这张表上**强不强制唯一性**的索引建在哪几列上.
    ///
    /// `UPDATE` 靠它决定"这次改动有没有可能撞键":`SET` 一列都没碰到这些列时,
    /// 唯一性**不可能**被破坏(那些列的值一个都没动),于是整表查重那一步可以
    /// 整段省掉--它是 O(整表),而 `tests.sql` / `update_*.sql` 里的 `UPDATE`
    /// 全是改无关列(`runtime_minutes` + 1 之类),这条省下来的是**每一次**语句.
    pub fn unique_columns_of(&self, table: &str) -> Vec<usize> {
        // 调用点(`UPDATE`)是热路径:名字只解一次,别在每次比较里再走一遍 `candidates`
        let table = self.effective_name(table);
        self.indexes
            .iter()
            .filter(|index| index.table == table && index.unique())
            .map(|index| index.column())
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

    /// `UPDATE` 之后让这张表上**写到的列**的索引失效(下次用到时重建).
    ///
    /// 为什么不是现场维护:一行的改动会挪动好几个键,逐个改与"整体作废,下次重建"
    /// 相比既容易错也不划算;`UPDATE` 本来就要扫全表.
    ///
    /// 为什么**不是作废全部**(TODO 3.15):`update_row` 是**原地替换**,行号一行都
    /// 不动--于是没被写到的列,它那份"键 → 行号"仍然**完全有效**,作废它就是下一
    /// 次用到时白重建一遍整表(那是 O(整表) 的).作废必须**先做**:中途出错也不能
    /// 留下拿旧行号回答新键的索引.
    pub fn invalidate_indexes(&mut self, table: &str, written: &[usize]) {
        let table = self.effective_name(table);
        for index in &self.indexes {
            if index.table == table && written.contains(&index.column()) {
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

        // 要删掉的表**搬**进 undo:`retain` 一走,这些表的数据就只剩 undo 里那一份
        // 了(不是克隆--删一个 schema 不该先把它的数据复制一遍).只回滚目录
        // (留下一个空壳名字)比不实现事务更糟.
        let doomed_slots: Vec<usize> = self
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| symbol.name().starts_with(&prefix))
            .map(|(index, _)| index)
            .collect();
        for index in doomed_slots {
            self.take_for_undo(index);
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
