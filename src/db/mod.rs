//! 数据库核心:表的抽象层与各种具体实现.
//!
//! * [`table`] -- 表的抽象:[`Table`] / [`TabShape`] / 值模型 / 错误.
//! * [`phys`] -- 物理表 [`PhysTab`].
//! * [`source`] -- 拉取式的中间结果 [`RowSource`] (与随机访问的 [`Table`] 并列).
//! * [`index`] -- 一列上的"键 → 行号"索引,以及把行号读成行的
//!   [`IndexSource`] (它也是一个 [`RowSource`]).
//!
//! 页式基表(数据在磁盘上,行按需解码)在 [`crate::pers::heap`]:它实现的是同一套
//! [`Table`],只是覆盖了 [`Table::paged`] 那条口子--"我自己落盘,不必整表重写".
//!
//! 连接**不再需要一个 `Table`**.有了 [`RowSource`] 之后,连接,哈希连接,
//! 索引扫描都是"又一个按行拉的源":只有需要随机访问或重扫的地方才物化成
//! [`PhysTab`].索引也一样--它只回答"哪些行是这个键",不复制行.

pub mod index;
pub mod phys;
pub mod source;
pub mod table;

pub use index::{Index, IndexSource};
pub use phys::PhysTab;
pub use source::{RowSource, TableCursor};
pub use table::{
    Column, ColumnsShape, DataType, DbError, Paged, Result, Row, TabShape, Table, Value,
};
