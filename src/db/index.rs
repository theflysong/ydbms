//! 索引：一个「键 → 行号」的哈希表，以及把它当 [`RowSource`] 用的游标。
//!
//! 索引**不是表**。[`Table`] 说的是随机访问（`nrows()` + `row(r)`），索引说的是
//! 「给定一个键，哪些行是它」——而 [`IndexSource`] 拿到那批行号之后，逐行向基表
//! 要行，于是它天然是**又一个按行拉的源**（见 [`crate::db::source`]）。这正是
//! 拉取式执行模型给的自由：没有任何算子要求「第 i 行是什么」，所以索引既能当
//! `Scan` 的访问路径，也能当连接内侧的查找结构，不必再有一个 `JoinTab: Table`。
//!
//! # 索引是**查询期**建的
//!
//! 本版没有 `CREATE INDEX`（[`crate::ir::lower`] 仍然显式拒绝它），索引由用它的
//! 人现建。于是有一条必须说清的代价：**建一份索引要先把表扫一遍**，所以单表
//! `WHERE x = 1` 并不比顺序扫描快。收益在别处：
//!
//! * 连接的每行一次查找——外侧 N 行就是 N 次查找，不再逐对配对；
//! * 内侧不必物化整行（索引只存键与行号）；
//! * 同一份索引被反复查找时，建它的那一遍被摊薄。
//!
//! 持久索引（或 `CREATE INDEX`）是把这条路径变成净收益的前提，见 `TODO.md` §3.6。
//!
//! # 键就是 [`Value`]
//!
//! 于是「同一个键」的定义与 `Eq`/`Hash` 一致：`NUMERIC` 的文本是规范化的、
//! 文本只有 `Value::Text` 一个变体。**跨大类的值不是同一个键**（`Int(1)` 与
//! `Numeric("1")`），所以调用方必须保证查的键与被索引的列同族——这一条由
//! [`crate::ir::Plan::analyze`] 静态强制，否则索引会安静地漏掉本该匹配的行。

use std::collections::HashMap;

use super::source::RowSource;
use super::table::{Column, DbError, Result, Row, Table, Value};

/// 一列上的哈希索引：键 → 行号（**行号升序**，也就是表顺序）。
///
/// `NULL` 也是一个键（`WHERE x IS NULL` 将来可以直接查它）；至于 `x = NULL`
/// 永远匹配不上任何行，那是**求值侧**的事，不由索引负责。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    column: usize,
    buckets: HashMap<Value, Vec<usize>>,
}

impl Index {
    /// 扫一遍表，给第 `column` 列建索引。
    ///
    /// 只读**这一列**（[`Table::value`]），不整行克隆——这是它比物化整表省的
    /// 地方。短读（`nrows()` 说有这一行、`value()` 却给不出来）算实现坏了，
    /// 报 [`DbError::IrRuntime`]，不当作读到结尾。
    pub fn build(table: &dyn Table, column: usize) -> Result<Self> {
        let arity = table.arity();
        if column >= arity {
            return Err(DbError::ColumnOutOfBounds { index: column, arity });
        }

        let mut buckets: HashMap<Value, Vec<usize>> = HashMap::new();
        for row in 0..table.nrows() {
            let key = table.value(row, column).ok_or_else(|| DbError::IrRuntime {
                node: None,
                message: format!("建索引时读不到第 {} 行第 {} 列", row, column),
            })?;
            buckets.entry(key).or_default().push(row);
        }

        Ok(Index { column, buckets })
    }

    /// 索引建在哪一列上。
    pub fn column(&self) -> usize {
        self.column
    }

    /// 这个键的行号，**升序**；没有这个键就是空切片。
    pub fn lookup(&self, key: &Value) -> &[usize] {
        self.buckets.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 一共多少个不同的键。
    pub fn nkeys(&self) -> usize {
        self.buckets.len()
    }
}

/// 索引给出的行号，一行一行地从基表读出来。
///
/// **索引是惰性建的**：第一次 [`RowSource::next`] 才建，所以「上层不拉，下面一行
/// 都不读」这条拉取式的性质在这里也成立（`LIMIT 0` 之下连索引都不会建）。
pub struct IndexSource<'a> {
    table: &'a dyn Table,
    column: usize,
    key: Value,
    /// 要读的行号；`None` 表示索引还没建。
    rids: Option<Vec<usize>>,
    at: usize,
    error: Option<DbError>,
}

impl<'a> IndexSource<'a> {
    /// 在 `table` 的第 `column` 列上按 `key` 取行。
    pub fn new(table: &'a dyn Table, column: usize, key: Value) -> Self {
        IndexSource {
            table,
            column,
            key,
            rids: None,
            at: 0,
            error: None,
        }
    }

    /// 建索引、结算出要读的行号。失败记进 `self.error`。
    fn fill(&mut self) -> Result<()> {
        let index = Index::build(self.table, self.column)?;
        self.rids = Some(index.lookup(&self.key).to_vec());
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

        let rids = self.rids.as_ref().expect("上面刚建过");
        let rid = *rids.get(self.at)?;
        self.at += 1;

        match self.table.row(rid) {
            Some(row) => Some(row),
            None => {
                self.error = Some(DbError::IrRuntime {
                    node: None,
                    message: format!("读不到第 {} 行", rid),
                });
                None
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref()
    }
}
