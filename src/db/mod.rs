//! 数据库核心：表的抽象层与各种具体实现。
//!
//! * [`table`] —— 表的抽象：[`Table`] / [`TabShape`] / 值模型 / 错误。
//! * [`phys`] —— 物理表 [`PhysTab`]。
//! * [`source`] —— 拉取式的中间结果 [`RowSource`]（与随机访问的 [`Table`] 并列）。
//!
//! 连接**不再需要一个 `Table`**。有了 [`RowSource`] 之后，连接（以及将来的哈希连接、
//! 索引扫描）都是「又一个按行拉的源」：只有需要随机访问或重扫的地方才物化成
//! [`PhysTab`]。

pub mod phys;
pub mod source;
pub mod table;

pub use phys::PhysTab;
pub use source::{RowSource, TableCursor};
pub use table::{Column, ColumnsShape, DataType, DbError, Result, Row, TabShape, Table, Value};
