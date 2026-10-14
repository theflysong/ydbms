//! 拉取式的中间结果：[`RowSource`]。
//!
//! [`Table`] 描述的是**随机访问**：`nrows()` + `row(r)`，问哪一行就给哪一行。那是基表
//! （以及落盘）要的契约。执行器要的是另一件事：**一行一行往前拉**，拉过的就丢掉。
//! 两件事拆成两个 trait——`Table` 管前者，[`RowSource`] 管后者，[`TableCursor`] 把
//! 前者当后者用。
//!
//! # 惰性边界
//!
//! 管道里只有这几处会把行**攒下来**（都走 [`materialize`]）：
//!
//! * 需要**随机访问**的算子——`Sort` 与 `Aggregate` 必须先看完全部输入；
//! * 需要**重扫**的地方——嵌套循环连接的内侧、被多个父节点引用的节点；
//! * 哈希连接的 build 侧（抽干成「键 → 行」）；
//! * **最终结果**（调用方拿到的 [`PhysTab`]）。
//!
//! 其余算子（`Scan` / `Filter` / `Project` / `Limit`）全程流式：上层要一行，才往下拉一行。
//!
//! **索引**（见 [`crate::db::index`]）攒的是**行号**而不是行，所以索引嵌套循环不物化
//! 内侧。
//!
//! # 结束与出错是两个状态
//!
//! [`RowSource::next`] 返回 `None` 既可能是**正常结束**，也可能是**出错**——原因在
//! [`RowSource::error`] 里。这样 trait 保持 §3.3 给的形状（`next` 只吐 `Option<Row>`），
//! 又不丢掉运行期错误。一旦结束或出错，后续 `next()` 必须继续返回 `None`（幂等）。

use crate::db::phys::PhysTab;
use crate::db::table::{Column, ColumnsShape, DbError, Result, Row, Table};

/// 中间结果：一行一行拉，不是一张物化的表。
///
/// 每个算子实现它，于是整条查询管道是一串**套起来的游标**：上层拉一行，下层才去算一行。
///
/// ```
/// use ydbms::db::table::{Column, ColumnsShape, DataType, TabShape, Table, Value};
/// use ydbms::db::source::{RowSource, TableCursor};
/// use ydbms::db::PhysTab;
///
/// let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
/// let mut tab = PhysTab::new("t", &shape);
/// tab.insert(vec![Value::Int(1)]).unwrap();
/// tab.insert(vec![Value::Int(2)]).unwrap();
///
/// let mut cursor = TableCursor::new(&tab);
/// assert_eq!(cursor.columns(), tab.columns());
/// assert_eq!(cursor.next(), Some(vec![Value::Int(1)]));
/// assert_eq!(cursor.next(), Some(vec![Value::Int(2)]));
/// // 结束之后一直结束，`error()` 说明这不是出错
/// assert_eq!(cursor.next(), None);
/// assert_eq!(cursor.next(), None);
/// assert!(cursor.error().is_none());
/// ```
pub trait RowSource {
    /// 诊断名。`Scan` 用它那张表的名字，其余算子用 `<filter>` 之类的标记。
    ///
    /// 最终结果表的 `name` 就取自 root 源，所以它是**可观察**的。
    fn name(&self) -> &str;

    /// 输出列。
    fn columns(&self) -> &[Column];

    /// 拉下一行；`None` 表示结束（**也许**是出错，见 [`RowSource::error`]）。
    ///
    /// 一旦结束或出错，后续调用必须继续返回 `None`——[`drain`] 的抽干循环与
    /// `Limit` 的跳过循环都依赖这一点。
    fn next(&mut self) -> Option<Row>;

    /// 上一次 `next` 返回 `None` 的原因；正常结束返回 `None`。
    ///
    /// 复合算子**不自己抄一份输入的错误**，而是往下问（`self.input.error()`）：
    /// 错误只可能来自真正被拉过的那一段，所以「上层提前停住、下层从没被拉过」
    /// 不会误报。默认实现是「没有错误」。
    fn error(&self) -> Option<&DbError> {
        None
    }
}

/// 基表的顺序游标：把随机访问的 [`Table`] 读成只进不退的一行一行。
///
/// 短读（`nrows()` 说有这么一行，`row()` 却给不出来）算**实现坏了**，报
/// [`DbError::IrRuntime`]，不当作读到结尾——本 crate 一直是这个口径。
///
/// ```
/// use ydbms::db::table::{Column, ColumnsShape, DataType, Table, Value};
/// use ydbms::db::source::{RowSource, TableCursor};
/// use ydbms::db::PhysTab;
///
/// let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
/// let mut tab = PhysTab::new("t", &shape);
/// tab.insert(vec![Value::Int(7)]).unwrap();
/// let mut cursor = TableCursor::new(&tab);
/// assert_eq!(cursor.name(), "t");
/// assert_eq!(cursor.next(), Some(vec![Value::Int(7)]));
/// assert_eq!(cursor.next(), None);
/// ```
pub struct TableCursor<'a> {
    table: &'a dyn Table,
    at: usize,
    error: Option<DbError>,
}

impl<'a> TableCursor<'a> {
    /// 从第一行开始扫 `table`。
    pub fn new(table: &'a dyn Table) -> Self {
        TableCursor {
            table,
            at: 0,
            error: None,
        }
    }
}

impl RowSource for TableCursor<'_> {
    fn name(&self) -> &str {
        self.table.name()
    }

    fn columns(&self) -> &[Column] {
        self.table.columns()
    }

    fn next(&mut self) -> Option<Row> {
        if self.error.is_some() || self.at >= self.table.nrows() {
            return None;
        }
        match self.table.row(self.at) {
            Some(row) => {
                self.at += 1;
                Some(row)
            }
            None => {
                self.error = Some(DbError::IrRuntime {
                    node: None,
                    message: format!("读取第 {} 行失败", self.at),
                });
                None
            }
        }
    }

    fn error(&self) -> Option<&DbError> {
        self.error.as_ref()
    }
}

/// 把整个源抽干成行。
///
/// 需要**重排**（`Sort`）或**重扫**（`Aggregate`、连接的内侧）之前必须先抽干。
/// 出错就返回错误，**不返回半截结果**。
pub fn drain(source: &mut dyn RowSource) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    while let Some(row) = source.next() {
        rows.push(row);
    }
    match source.error() {
        Some(error) => Err(error.clone()),
        None => Ok(rows),
    }
}

/// 把整个源直接抽干成一张物化表，名字与列取自源。
///
/// 最终结果与「被多个父节点引用」的节点走它；逐行走 [`PhysTab::insert`]，
/// 于是元数 / `NOT NULL` / typmod 的校验与过去一致。
pub fn materialize(source: &mut dyn RowSource) -> Result<PhysTab> {
    let mut table = PhysTab::new(source.name(), &ColumnsShape::new(source.columns().to_vec()));
    while let Some(row) = source.next() {
        table.insert(row)?;
    }
    match source.error() {
        Some(error) => Err(error.clone()),
        None => Ok(table),
    }
}
