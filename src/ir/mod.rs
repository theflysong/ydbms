//! 线性 IR。
//!
//! 这一层把 SQL 变成"要查什么"，并且**两层都没有递归**：
//!
//! ```text
//! SQL 文本
//!    │ pg_query::parse（PostgreSQL 的真实解析器）
//!    ▼
//! pg_query 裸语法树
//!    │ lower()                          ← lower.rs   绑定：查表、解列名、类型检查
//!    ▼
//! Plan { nodes: Vec<Node>, root }       ← plan.rs    节点之间的边是「下标」
//!    │  每个节点的算子各带若干
//!    ▼
//! Prog { steps: Vec<Step> }             ← expr.rs    一小段寄存器程序
//!    │
//!    ▼
//! RowSource（一行一行拉）                ← exec.rs    拉取式执行
//!    │  只有随机访问 / 重扫 / 最终结果才物化
//!    ▼
//! PhysTab                               ← db::source::materialize
//! ```
//!
//! 执行是**拉取式**的：每个节点产出一个 [`RowSource`](crate::db::source::RowSource)，
//! 上层拉一行下层才算一行；[`execute_source`] 把「拉多少、什么时候停」交给调用方，
//! [`execute`] 则抽干 root 返回结果表。
//!
//! 名字 → 表的映射由 [`SymbolTable`]（`symbol.rs`）提供。
//!
//! # 两条不变量
//!
//! * **I1（拓扑序）**：[`Node::inputs`] 里的下标必须**严格小于**节点自身的下标。
//!   它保证图无环，并且让 schema 推导与执行都成为一次正向 `for` 循环。
//! * **I2（先定义后使用）**：[`Step`] 读到的寄存器必须在此前被写过。
//!
//! 两条都由 [`Plan::analyze`] 静态检查。降级出来的计划在交给执行器之前一定会
//! 过这一关——所以就算绑定器算错了列下标，也会以
//! [`crate::db::table::DbError::IrInvalid`] 被抓住，而不是安静地读错列。
//!
//! # 已实现 / 未实现
//!
//! 已实现：`SELECT`（多表内连接与左外连接、`WHERE`、`ORDER BY`、`LIMIT`/`OFFSET`）、
//! 聚合与 `GROUP BY`/`HAVING`、`CASE`、`ROUND`、`CREATE TABLE`、`INSERT`、
//! NULL 与三值逻辑、`INT`/`TEXT`/`CHAR`/`VARCHAR`/`TIMESTAMP`/`NUMERIC`，
//! 以及**拉取式**的执行（见 [`exec`]）。
//!
//! 未实现：**优化器**——降级出来的就是朴素计划，没有重排、没有谓词下推、没有
//! 代价估算；子查询；集合运算（`UNION` 等）；CTE；`DISTINCT`；窗口函数；
//! `RIGHT`/`FULL` JOIN；`UPDATE`/`DELETE`/`DROP`/`ALTER`；事务。

pub mod exec;
pub mod expr;
pub mod lower;
pub mod plan;
pub mod stmt;
pub mod symbol;

pub use exec::{execute, execute_source};
pub use expr::{BinOp, EvalError, Prog, ProgBuilder, Reg, Scalar, ScalarType, Step, UnOp};
pub use plan::{Analysis, JoinKind, Node, NodeId, Op, Plan, PlanBuilder, SortKey};
pub use stmt::{
    execute_stmt, has_top_level_terminator, run_script, Outcome, Script, Stmt, StmtResult,
};
pub use symbol::{Symbol, SymbolTable};

use crate::db::table::Result;

/// 把一段 SQL 解析并降级成线性 IR。
///
/// 这是给外部用的入口（`main.rs` 之类的调用方）；需要一步步来的话用
/// [`lower::lower`] 与 [`lower::lower_statement`]。
///
/// 降级本身**不执行**任何东西，也不会碰数据——要拿结果得再调 [`execute`]。
///
/// ```
/// use ydbms::db::table::{Column, ColumnsShape, DataType, Table, Value};
/// use ydbms::db::PhysTab;
/// use ydbms::ir::{compile, execute, SymbolTable};
///
/// let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
/// let mut tab = PhysTab::new("t", &shape);
/// tab.insert(vec![Value::Int(1)]).unwrap();
/// tab.insert(vec![Value::Int(2)]).unwrap();
///
/// let mut symbols = SymbolTable::new();
/// symbols.define("t", tab).unwrap();
///
/// let plan = compile("SELECT x FROM t WHERE x > 1", &symbols).unwrap();
/// let rows = execute(&plan, &symbols).unwrap();
/// assert_eq!(rows.nrows(), 1);
/// ```
pub fn compile(sql: &str, symbols: &SymbolTable) -> Result<Plan> {
    lower::lower(sql, symbols)
}
