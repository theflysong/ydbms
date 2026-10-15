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
//! * `UPDATE` 之后索引失效,下一次用到它时重建一份(只重建一次);
//! * 没有索引的列**不建索引**:查询就是顺序扫描--本版不会为了"也许有用"先把表
//!   扫一遍(`CREATE INDEX` 才是让人掏这笔钱的地方).
//!
//! # 键就是 [`Value`]
//!
//! 于是"同一个键"的定义与 `Eq`/`Hash` 一致:`NUMERIC` 的文本是规范化的,
//! 文本只有 `Value::Text` 一个变体.**跨大类的值不是同一个键**(`Int(1)` 与
//! `Numeric("1")`),所以调用方必须保证查的键与被索引的列同族--这一条由
//! [`crate::ir::Plan::analyze`] 静态强制,否则索引会安静地漏掉本该匹配的行.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::source::RowSource;
use super::table::{Column, DbError, Result, Row, Table, Value};

/// 一列上的哈希索引:键 → 行号(**行号升序**,也就是表顺序).
///
/// `NULL` 也是一个键(`WHERE x IS NULL` 将来可以直接查它);至于 `x = NULL`
/// 永远匹配不上任何行,那是**求值侧**的事,不由索引负责.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    column: usize,
    buckets: HashMap<Value, Vec<usize>>,
}

impl Index {
    /// 扫一遍表,给第 `column` 列建索引.
    ///
    /// 只读**这一列**([`Table::value`]),不整行克隆--这是它比物化整表省的
    /// 地方.短读(`nrows()` 说有这一行,`value()` 却给不出来)算实现坏了,
    /// 报 [`DbError::IrRuntime`],不当作读到结尾.
    pub fn build(table: &dyn Table, column: usize) -> Result<Self> {
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
            buckets.entry(key).or_default().push(row);
        }

        Ok(Index { column, buckets })
    }

    /// 索引建在哪一列上.
    pub fn column(&self) -> usize {
        self.column
    }

    /// 这个键的行号,**升序**;没有这个键就是空切片.
    pub fn lookup(&self, key: &Value) -> &[usize] {
        self.buckets.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 一共多少个不同的键.
    pub fn nkeys(&self) -> usize {
        self.buckets.len()
    }

    /// 追加一行(`INSERT` 之后维护索引用).
    ///
    /// 不校验键与列是否同族:键是从**那一列自己**读出来的(见
    /// [`IndexSlot::insert_row`] 的调用点),同族是构造上的事实.
    pub fn insert(&mut self, key: Value, rid: usize) {
        self.buckets.entry(key).or_default().push(rid);
    }
}

/// 一列上的**索引槽位**:一份可以失效,可以重建的索引.
///
/// 它被 [`Rc`] 共享给正在跑的游标,所以内部状态放在 [`RefCell`] 里:目录是
/// `&SymbolTable` 借出去的,而"失效 / 重建 / 追加一行"都要改它.
#[derive(Debug, Default)]
pub struct IndexSlot {
    column: usize,
    built: RefCell<Option<Index>>,
}

impl IndexSlot {
    /// 一个还没建的空槽位(属于第 `column` 列).
    pub fn new(column: usize) -> Self {
        IndexSlot {
            column,
            built: RefCell::new(None),
        }
    }

    /// 索引建在哪一列.
    pub fn column(&self) -> usize {
        self.column
    }

    /// 现在建好一份(已经建过就重建).`CREATE INDEX` 走它.
    pub fn build(&self, table: &dyn Table) -> Result<()> {
        let index = Index::build(table, self.column)?;
        *self.built.borrow_mut() = Some(index);
        Ok(())
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
    pub fn insert_row(&self, key: Value, rid: usize) {
        if let Some(index) = self.built.borrow_mut().as_mut() {
            index.insert(key, rid);
        }
    }

    /// 作废(`UPDATE` 写完表之后).下次用到时重建.
    pub fn invalidate(&self) {
        *self.built.borrow_mut() = None;
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
pub struct IndexSource<'a> {
    table: &'a dyn Table,
    /// 这一列上的索引槽位:目录里的那一个(共享),或者执行器临时建的一个.
    slot: Rc<IndexSlot>,
    key: Value,
    /// 要读的行号;`None` 表示还没查过.
    rids: Option<Vec<usize>>,
    at: usize,
    error: Option<DbError>,
}

impl<'a> IndexSource<'a> {
    /// 在 `table` 上按 `key` 取行,索引放在 `slot` 里.
    pub fn new(table: &'a dyn Table, slot: Rc<IndexSlot>, key: Value) -> Self {
        IndexSource {
            table,
            slot,
            key,
            rids: None,
            at: 0,
            error: None,
        }
    }

    /// 查一次键,结算出要读的行号.失败记进 `self.error`.
    fn fill(&mut self) -> Result<()> {
        self.rids = Some(self.slot.lookup(self.table, &self.key)?);
        Ok(())
    }
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
