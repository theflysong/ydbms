//! 表级 `CHECK` 的强制点:一层**装饰器**.
//!
//! `db` 层不认识 [`Prog`] (它是 IR 的类型),所以 `CHECK` 不能写进
//! [`PhysTab::validate`](crate::db::phys::PhysTab)--那一层只管 typmod 与
//! `NOT NULL`.这里的做法是:`CREATE TABLE` 带 `CHECK` 时,把 `PhysTab` 包进
//! [`CheckedTable`],于是**所有**写路径(`INSERT` 与将来的 `UPDATE`)都自动过
//! 约束--它们都是通过 [`Table::insert`] / [`Table::update_row`] 进来的.
//!
//! # 只有 FALSE 算违规
//!
//! `CHECK` 的谓词是三值逻辑:`CHECK (died + 0 = died)` 在 `died IS NULL` 时
//! 结果是 `Unknown`.PostgreSQL 的语义是"**不是假**就通过",所以这里也只在
//! `Tri::False` 时报 [`DbError::CheckViolation`].
//!
//! # 约束不落盘
//!
//! `CHECK` 是 [`Prog`],模式文件里存不下(那要序列化整个 IR).所以
//! `CREATE TABLE` 建的约束只在**本会话**有效,`pers` 读回来的表没有 `CHECK`--
//! 与 `CREATE INDEX`,`DEFAULT` 是同一类限制,已在各自文档里写明.

use crate::db::phys::PhysTab;
use crate::db::table::{Column, DbError, Result, Row, Table, TabShape, Value};

use super::expr::Prog;
use super::value::Tri;

/// 一条 `CHECK` 约束.
///
/// `name` 是**报错时用的名字**:语句里写了 `CONSTRAINT "x"` 就是 `x`,否则是建表时
/// 按 PostgreSQL 的口径合成的(`<表>_<列>_check` / `<表>_check`).约束的表达式
/// 原文没有留下来--它已经被降级成 `program`,而本版没有把 IR 渲染回 SQL 的东西.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// 约束名(写了就用写的,没写就是合成名).
    pub name: String,
    /// 谓词程序;面对**整张表**的扁平 schema(列序就是表的列序).
    pub program: Prog,
}

impl Check {
    /// 这一行满足约束吗.
    ///
    /// 只有确定的假返回 `Ok(false)`;`Unknown`(NULL 谓词结果)**放行**.
    fn holds(&self, row: &[Value]) -> Result<bool> {
        let value = self
            .program
            .eval(row)
            .map_err(|e| e.into_db_error(None))?;
        match value.as_truth() {
            Some(Tri::False) => Ok(false),
            Some(_) => Ok(true),
            // 静态校验已经保证 CHECK 是布尔表达式;这里只是不让它变成"长得一样的结果"
            None => Err(DbError::IrRuntime {
                node: None,
                message: format!("{}", tr!("ir.checks.001", arg0 =  value)),
            }),
        }
    }

}

/// 带 `CHECK` 的表:写之前先过约束,其余原样转发给里面的 [`PhysTab`].
///
/// 只包 `PhysTab` 而不是 `Box<dyn Table>`:`CHECK` 只对真实存在的表有意义
/// (派生表,连接结果都不接受写入).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedTable {
    inner: PhysTab,
    checks: Vec<Check>,
}

impl CheckedTable {
    /// 给一张物理表挂上若干条 `CHECK`.
    pub fn new(inner: PhysTab, checks: Vec<Check>) -> Self {
        CheckedTable { inner, checks }
    }

    /// 里面的物理表.
    pub fn inner(&self) -> &PhysTab {
        &self.inner
    }

    /// 这张表上的约束.
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// 候选行过一遍所有约束.
    fn check(&self, row: &Row) -> Result<()> {
        for check in &self.checks {
            if !check.holds(row)? {
                return Err(DbError::CheckViolation {
                    table: self.inner.name().to_string(),
                    constraint: check.name.clone(),
                });
            }
        }
        Ok(())
    }
}

impl TabShape for CheckedTable {
    fn columns(&self) -> &[Column] {
        self.inner.columns()
    }
}

impl Table for CheckedTable {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn nrows(&self) -> usize {
        self.inner.nrows()
    }

    fn row(&self, r: usize) -> Option<Row> {
        self.inner.row(r)
    }

    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.inner.value(r, c)
    }

    fn insert(&mut self, row: Row) -> Result<usize> {
        self.check(&row)?;
        self.inner.insert(row)
    }

    fn update(&mut self, r: usize, c: usize, value: Value) -> Result<()> {
        let nrows = self.inner.nrows();
        let mut row = self
            .inner
            .row(r)
            .ok_or(DbError::RowOutOfBounds { index: r, nrows })?;
        let arity = row.len();
        if c >= arity {
            return Err(DbError::ColumnOutOfBounds { index: c, arity });
        }
        row[c] = value.clone();
        self.check(&row)?;
        self.inner.update(r, c, value)
    }

    /// 快照必须把 `CHECK` 一起带走:默认实现物化成一张 `PhysTab`,
    /// 那样回滚之后的表就不再强制约束了.
    fn snapshot(&self) -> Box<dyn Table> {
        Box::new(self.clone())
    }

    fn update_row(&mut self, r: usize, row: Row) -> Result<()> {
        self.check(&row)?;
        self.inner.update_row(r, row)
    }

    fn delete(&mut self, r: usize) -> Result<()> {
        self.inner.delete(r)
    }
}
