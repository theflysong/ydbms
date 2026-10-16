//! 索引:一个"键 → 行号"的哈希表,以及把它当 [`RowSource`] 用的游标.
//!
//! 索引**不是表**.[`Table`] 说的是随机访问(`nrows()` + `row(r)`),索引说的是
//! "给定一个键,哪些行是它"--而 [`IndexSource`] 拿到那批行号之后,逐行向基表
//! 要行,于是它天然是**又一个按行拉的源**(见 [`crate::db::source`]).这正是
//! 拉取式执行模型给的自由:没有任何算子要求"第 i 行是什么",所以索引既能当
//! `Scan` 的访问路径,也能当连接内侧的查找结构,不必再有一个 `JoinTab: Table`.
//!
//! # 索引是**目录对象**,由 `CREATE INDEX` 建
//!
//! [`IndexSlot`] 是"一列上的一份索引":它能**失效**(`UPDATE` 写完表就作废),
//! 也能**重建**(下次用到时按需建一次).名字与它属于哪张表记在符号表里
//! ([`crate::ir::symbol`]),这里只管"列 + 当前那一份".
//!
//! 于是代价的口径很清楚:
//!
//! * `CREATE INDEX` 立刻扫一遍表把索引建出来(这笔代价这时候付);
//! * `INSERT` 只把新行**追加**进去(不重建);
//! * `UPDATE` 只让**写到的那些列**上的索引失效(TODO 3.15:`update_row` 是原地替换,
//!   行号不动,别的列那份"键 → 行号"仍然有效),下一次用到它时重建一份(只重建一次);
//! * 没有索引的列**不建索引**:查询就是顺序扫描--本版不会为了"也许有用"先把表
//!   扫一遍(`CREATE INDEX` 才是让人掏这笔钱的地方).
//!
//! # 一次查**一组**键(TODO 3.14(a))
//!
//! [`IndexSource`] 收的是**一组**键而不是一个:`col IN (常量, ...)` 就是好几个键,
//! 各键的桶合并([`lookup_many`])起来就是命中的行号(行号升序 = 表顺序,重复的键
//! 只算一次).不这么办的话,那几个键要对**每一行**各比一次--实测 `update_1k.sql`
//! 第一条(`name_basics` 50 万行,1000 个键)因此要 84 秒,走索引之后是 0.37 秒.
//!
//! # `unique`:主键那一份**真强制**唯一性(TODO 8.7)
//!
//! 同样一份"键 → 行号",但 [`IndexSlot::unique`] 是 `true` 的那一份会被语句层
//! 当成唯一性的依据:
//!
//! * `INSERT` / `COPY`:每一行落库之前,按"这一句见过哪些键"的台账查一次重
//!   (台账的起点是 [`IndexSlot::keys`],见 `crate::ir::stmt` 的 `IndexKeys`);
//! * `UPDATE`:写到的列上的索引作废之后当场按新数据查一遍重,不让重键留到下一次
//!   查询才冒出来;
//! * 重建:唯一索引建在一张**已经有重复行**的表上,建都建不出来([`Index::build`]).
//!
//! 主键那份是隐含的 `unique` 索引来源(`CREATE TABLE ... PRIMARY KEY`,单列);
//! `CREATE UNIQUE INDEX` 也走同一条路.**`UNIQUE` 列上的索引不是** `unique` 的:
//! 它只当访问路径,重复键照样放行--这是本版记录在案的偏差,口径写在
//! `src/ir/AGENTS.md`.
//!
//! # 键就是 [`Value`]
//!
//! 于是"同一个键"的定义与 `Eq`/`Hash` 一致:`NUMERIC` 的文本是规范化的,
//! 文本只有 `Value::Text` 一个变体.**跨大类的值不是同一个键**(`Int(1)` 与
//! `Numeric("1")`),所以调用方必须保证查的键与被索引的列同族--这一条由
//! [`crate::ir::Plan::analyze`] 静态强制,否则索引会安静地漏掉本该匹配的行.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use super::source::RowSource;
use super::table::{Column, DbError, Result, Row, Table, Value};

/// 一列上的哈希索引:键 → 行号(**行号升序**,也就是表顺序).
///
/// `NULL` 也是一个键(`WHERE x IS NULL` 将来可以直接查它);至于 `x = NULL`
/// 永远匹配不上任何行,那是**求值侧**的事,不由索引负责.
///
/// # `unique`:主键那一份才强制唯一性(TODO 8.7)
///
/// 同样是"键 → 行号",但 [`IndexSlot::insert_row`] / [`IndexSlot::verify_unique`]
/// 在 `unique` 的索引上会**拒绝**重复键,报 [`DbError::DuplicateKey`].
///
/// 为什么不是"所有索引都强制":`UNIQUE` 列上的索引**只当访问路径**(重复键仍然
/// 放行,这是本版长期记录在案的偏差),主键那份才强制.差别就靠这一个标记,
/// 而它跟着 [`IndexSlot`] 走,所以"哪一份索引"与"强不强制"永远是同一件事.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    column: usize,
    unique: bool,
    buckets: HashMap<Value, Vec<usize>>,
}

impl Index {
    /// 扫一遍表,给第 `column` 列建索引.
    ///
    /// 只读**这一列**([`Table::value`]),不整行克隆--这是它比物化整表省的
    /// 地方.短读(`nrows()` 说有这一行,`value()` 却给不出来)算实现坏了,
    /// 报 [`DbError::IrRuntime`],不当作读到结尾.
    ///
    /// `unique` 时**当场**查重:同一个键落在两个行号上就报
    /// [`DbError::DuplicateKey`].于是"把一条唯一索引建在一张已经有重复行的表上"
    /// 不会先建出一份自相矛盾的索引再算账--建不出来就是建不出来.报错里的
    /// `index` 用 `name`(生效索引名),那一列的名字从表上取.
    pub fn build(table: &dyn Table, column: usize, unique: bool, name: &str) -> Result<Self> {
        let arity = table.arity();
        if column >= arity {
            return Err(DbError::ColumnOutOfBounds { index: column, arity });
        }

        let mut buckets: HashMap<Value, Vec<usize>> = HashMap::new();
        for row in 0..table.nrows() {
            let key = table.value(row, column).ok_or_else(|| DbError::IrRuntime {
                node: None,
                message: format!("{}", tr!("db.index.001", arg0 =  row, arg1 =  column)),
            })?;
            if unique && buckets.contains_key(&key) {
                return Err(DbError::DuplicateKey {
                    index: name.to_string(),
                    column: column_name(table, column),
                    value: format!("{}", key),
                });
            }
            buckets.entry(key).or_default().push(row);
        }

        Ok(Index {
            column,
            unique,
            buckets,
        })
    }

    /// 索引建在哪一列上.
    pub fn column(&self) -> usize {
        self.column
    }

    /// 这一份索引强制唯一性吗(只有主键那份是).
    pub fn unique(&self) -> bool {
        self.unique
    }

    /// 这个键已经在索引里了吗.
    ///
    /// 只看**有没有键**,不看那个桶里有多少行:唯一性的判据是"这个值出现过",
    /// 不是"这个值只出现过一次".
    pub fn contains(&self, key: &Value) -> bool {
        self.buckets.contains_key(key)
    }

    /// 这个键的行号,**升序**;没有这个键就是空切片.
    pub fn lookup(&self, key: &Value) -> &[usize] {
        self.buckets.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 一共多少个不同的键.
    pub fn nkeys(&self) -> usize {
        self.buckets.len()
    }

    /// 追加一个键 → 行号.
    ///
    /// 不校验键与列是否同族:键是从**那一列自己**读出来的(见
    /// [`IndexSlot::insert_row`] 的调用点),同族是构造上的事实.
    ///
    /// 这里**不查重**:唯一性的台账在语句层(见 `crate::ir::stmt` 的
    /// `IndexKeys`),这里只管"把这一行挂到它的键上".
    pub fn insert(&mut self, key: Value, rid: usize) {
        self.buckets.entry(key).or_default().push(rid);
    }
}

/// 第 `column` 列的**列名**,取不到就拿列号顶上.
///
/// 只为了把 [`DbError::DuplicateKey`] 写得像 PostgreSQL 那样.
fn column_name(table: &dyn Table, column: usize) -> String {
    table
        .columns()
        .get(column)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| column.to_string())
}

/// 一列上的**索引槽位**:一份可以失效,可以重建的索引.
///
/// 它被 [`Rc`] 共享给正在跑的游标,所以内部状态放在 [`RefCell`] 里:目录是
/// `&SymbolTable` 借出去的,而"失效 / 重建 / 追加一行"都要改它.
///
/// `unique` 是**建这个槽位的人**给的:`CREATE TABLE ... PRIMARY KEY` 与
/// `CREATE UNIQUE INDEX` 给 `true`,别的给 `false`(见 [`Index::unique`]).
#[derive(Debug, Default)]
pub struct IndexSlot {
    column: usize,
    /// 这一份索引强不强制唯一性吗(只有主键 / `CREATE UNIQUE INDEX` 那份是).
    unique: bool,
    /// 这份索引的名字(生效名).只为了把 [`DbError::DuplicateKey`] 报得像
    /// PostgreSQL 那样--"哪一条唯一约束被撞了"要从这里说.
    name: String,
    built: RefCell<Option<Index>>,
}

impl IndexSlot {
    /// 一个还没建的空槽位(属于第 `column` 列,`unique` 决定它强不强制唯一性,
    /// `name` 是它在目录里的名字).
    pub fn new(column: usize, unique: bool, name: impl Into<String>) -> Self {
        IndexSlot {
            column,
            unique,
            name: name.into(),
            built: RefCell::new(None),
        }
    }

    /// 索引建在哪一列.
    pub fn column(&self) -> usize {
        self.column
    }

    /// 这一份索引强制唯一性吗.
    pub fn unique(&self) -> bool {
        self.unique
    }

    /// 现在建好一份(已经建过就重建).`CREATE INDEX` 走它.
    ///
    /// `unique` 的槽位在这里**顺带查重**:`Index::build` 那一趟本来就在逐行读
    /// 这一列,所以"把唯一索引建在有重复行的表上"当场就报
    /// [`DbError::DuplicateKey`].
    pub fn build(&self, table: &dyn Table) -> Result<()> {
        let index = Index::build(table, self.column, self.unique, &self.name)?;
        *self.built.borrow_mut() = Some(index);
        Ok(())
    }

    /// 这份索引的名字(生效名);手写的槽位可能是空串.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 保证有索引,然后查这个键的行号.
    ///
    /// **惰性**:第一次调用才建(失效之后也一样),于是"上层不拉,下面一行都不读"
    /// 这条拉取式的性质在索引这里也成立.
    pub fn lookup(&self, table: &dyn Table, key: &Value) -> Result<Vec<usize>> {
        if self.built.borrow().is_none() {
            self.build(table)?;
        }
        let built = self.built.borrow();
        let index = built.as_ref().expect(&tr!("db.index.002").into_owned());
        Ok(index.lookup(key).to_vec())
    }

    /// 追加一行(`INSERT` 之后维护索引).
    ///
    /// 索引**已经失效**(或者还没建)就什么都不做:下次用到时按需重建一遍,
    /// 于是这里不必为了维护而扫一次表.
    ///
    /// 唯一性**不在这里查**:强制点是语句层那句"这一句见过哪些键"的台账
    /// (见 `crate::ir::stmt` 的 `IndexKeys`),`INSERT` / `COPY` / `UPDATE` 共用
    /// 同一套判据,而且索引**失效着**也不会漏掉强制.
    pub fn insert_row(&self, key: Value, rid: usize) {
        if let Some(index) = self.built.borrow_mut().as_mut() {
            index.insert(key, rid);
        }
    }

    /// 作废(`UPDATE` 写完表之后).下次用到时重建.
    pub fn invalidate(&self) {
        *self.built.borrow_mut() = None;
    }

    /// 现在这一份索引里的**全部键**;还没建(或已失效)就是 `None`.
    ///
    /// 只读,不触发重建--语句层拿它给"这一句见过哪些键"的台账起个头(见
    /// `crate::ir::stmt` 的 `IndexKeys`).
    pub fn keys(&self) -> Option<BTreeSet<Value>> {
        let built = self.built.borrow();
        Some(built.as_ref()?.buckets.keys().cloned().collect())
    }

    /// 已经建好了吗(测试与报告用).
    pub fn is_built(&self) -> bool {
        self.built.borrow().is_some()
    }

    /// 建好的索引里有多少个不同的键;还没建就是 `None`.
    pub fn nkeys(&self) -> Option<usize> {
        self.built.borrow().as_ref().map(Index::nkeys)
    }
}

/// 索引给出的行号,一行一行地从基表读出来.
///
/// **索引是惰性建的**:槽位里还没有索引时,第一次 [`RowSource::next`] 才建,
/// 所以 `LIMIT 0` 之下连索引都不会建.
///
/// 键是**一组**(TODO 3.14(a)):等值那种只有一个,`IN` 列表有好几个--各键的桶
/// 合并起来([`lookup_many`])才是要读的行号.
pub struct IndexSource<'a> {
    table: &'a dyn Table,
    /// 这一列上的索引槽位:目录里的那一个(共享),或者执行器临时建的一个.
    slot: Rc<IndexSlot>,
    keys: Vec<Value>,
    /// 要读的行号;`None` 表示还没查过.
    rids: Option<Vec<usize>>,
    at: usize,
    error: Option<DbError>,
}

impl<'a> IndexSource<'a> {
    /// 在 `table` 上按这一组 `keys` 取行,索引放在 `slot` 里.
    pub fn new(table: &'a dyn Table, slot: Rc<IndexSlot>, keys: Vec<Value>) -> Self {
        IndexSource {
            table,
            slot,
            keys,
            rids: None,
            at: 0,
            error: None,
        }
    }

    /// 查一次键,结算出要读的行号.失败记进 `self.error`.
    fn fill(&mut self) -> Result<()> {
        self.rids = Some(lookup_many(&self.slot, self.table, &self.keys)?);
        Ok(())
    }
}

/// 一组键的行号:各键的桶**合并**,行号升序(**行号升序 = 表顺序**,输出顺序就是
/// 它),重复的键只算一次.
///
/// 一个键时**不做**排序:桶本来就是按行号追加出来的(升序),`IN` 那种才会走到
/// 排序 + 去重上--键可以重复(`a IN (1, 1)` 是合法的),不去的活同一行会被读两遍.
pub fn lookup_many(slot: &IndexSlot, table: &dyn Table, keys: &[Value]) -> Result<Vec<usize>> {
    if let [key] = keys {
        return slot.lookup(table, key);
    }

    let mut rids = Vec::new();
    for key in keys {
        rids.extend(slot.lookup(table, key)?);
    }
    rids.sort_unstable();
    rids.dedup();
    Ok(rids)
}

impl RowSource for IndexSource<'_> {
    fn name(&self) -> &str {
        self.table.name()
    }

    fn columns(&self) -> &[Column] {
        self.table.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() {
            return None;
        }

        if self.rids.is_none() {
            if let Err(e) = self.fill() {
                self.error = Some(e);
                return None;
            }
        }

        let rids = self.rids.as_ref().expect(&tr!("db.index.003").into_owned());
        let rid = *rids.get(self.at)?;
        self.at += 1;

        match self.table.row(rid) {
            Some(row) => Some(row),
            None => {
                self.error = Some(DbError::IrRuntime {
                    node: None,
                    message: format!("{}", tr!("db.index.004", arg0 =  rid)),
                });
                None
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref()
    }
}
