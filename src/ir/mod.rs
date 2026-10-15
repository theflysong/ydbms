//! 线性 IR.
//!
//! 这一层把 SQL 变成"要查什么",并且**两层都没有递归**:
//!
//! ```text
//! SQL 文本
//!    │ pg_query::parse(PostgreSQL 的真实解析器)
//!    ▼
//! pg_query 裸语法树
//!    │ lower()                          ← irgen.rs   绑定:查表,解列名,类型检查
//!    ▼
//! Plan { nodes: Vec<Node>, root }       ← plan.rs    节点之间的边是"下标"
//!    │  每个节点的算子各带若干
//!    ▼
//! Prog { steps: Vec<Step> }             ← expr.rs    一小段寄存器程序
//!    │
//!    ▼
//! RowSource(一行一行拉)                ← exec.rs    拉取式执行
//!    │  只有随机访问 / 重扫 / 最终结果才物化
//!    ▼
//! PhysTab                               ← db::source::materialize
//! ```
//!
//! 执行是**拉取式**的:每个节点产出一个 [`RowSource`](crate::db::source::RowSource),
//! 上层拉一行下层才算一行;[`execute_source`] 把"拉多少,什么时候停"交给调用方,
//! [`execute`] 则抽干 root 返回结果表.
//!
//! 名字 → 表/索引的映射由 [`SymbolTable`](`symbol.rs`)提供.
//!
//! # 文件分工
//!
//! * `value.rs` -- 值层:[`Scalar`] / [`Tri`] / [`ScalarType`](`Value` 本身在
//!   `db::table`,见那里的说明);
//! * `irgen.rs` -- 降级(绑定器):查表,解列名,挑访问路径;
//! * `opt/` -- **优化器**:谓词推到哪张表,连接怎么执行(`ir` 内部模块);
//! * `expr.rs` -- 表达式 IR 与求值;`plan.rs` -- 计划与静态校验;`exec.rs` --
//!   拉取式执行;`stmt.rs` -- 语句层(DDL/DML);`checks.rs` -- `CHECK` 的强制点;
//! * `symbol.rs` -- 符号表(表,别名,索引,schema/search_path).
//!
//! # 两条不变量
//!
//! * **I1(拓扑序)**:[`Node::inputs`] 里的下标必须**严格小于**节点自身的下标.
//!   它保证图无环,并且让 schema 推导与执行都成为一次正向 `for` 循环.
//! * **I2(先定义后使用)**:[`Step`] 读到的寄存器必须在此前被写过.
//!
//! 两条都由 [`Plan::analyze`] 静态检查.降级出来的计划在交给执行器之前一定会
//! 过这一关--所以就算绑定器算错了列下标,也会以
//! [`crate::db::table::DbError::IrInvalid`] 被抓住,而不是安静地读错列.
//!
//! 子查询还多一条(同样由 [`Plan::analyze`] 检查):**`SubPlan::outers` 必须与
//! 执行时压着的那几层外层行逐层对上**(`outers[0]` 就是所在节点程序面对的那份
//! schema).绑定器算错外层下标只有这里抓得住--[`Prog::verify`] 看不到符号表,
//! 也就看不到外层是什么.
//!
//! # 已实现 / 未实现
//!
//! 已实现:`SELECT`(多表内连接与左外连接,`WHERE`,`ORDER BY` 含**输出列序号**,
//! `LIMIT`/`OFFSET`,`DISTINCT`),聚合与 `GROUP BY`/`HAVING`/`COUNT(*) FILTER`,
//! `CASE`,`IN`/`BETWEEN`/`LIKE`/`ILIKE`/`~*`,`ROUND`/`UPPER`/`LEFT`/`CONCAT_WS`/
//! `LENGTH`/`EXTRACT(YEAR FROM ...)`/`CURRENT_DATE`,`::numeric`,
//! `CREATE TABLE`(`NOT NULL` / `DEFAULT` / `CHECK` 真强制;`PRIMARY KEY` / `UNIQUE` /
//! `FOREIGN KEY` 接受并**如实记录为未强制**),`INSERT`,`UPDATE`,
//! `CREATE INDEX` / `DROP INDEX`,`UNION [ALL]`,
//! **子查询**(标量 / `IN` / `EXISTS` / 相关引用 / 派生表 / `ANY`/`ALL`),
//! NULL 与三值逻辑,`INT`/`TEXT`/`CHAR`/`VARCHAR`/`TIMESTAMP`/`DATE`/`NUMERIC`,
//! **拉取式**的执行(见 [`exec`]),以及访问路径与连接上的几件事:
//!
//! * **谓词下推**:`WHERE` 的合取项只引用一个 FROM 项时推进那张表的 `Scan.filter`;
//!   `LEFT JOIN` 的**可空侧**只放行 null-rejecting 的谓词(`b.y = 5`,
//!   `b.y IS NOT NULL`),而且连接之上仍留一份--外连接补出来的 NULL 行没被那张表
//!   扫过,只有上面的谓词丢得掉它们.含子查询/外层引用的合取项一律不推;
//! * **索引**:`Scan` 可以带一条索引路径(`<列> = <不读列的表达式>`),走的是
//!   `CREATE INDEX` 建在目录里的那份索引([`crate::db::index::IndexSlot`]);
//! * **连接策略**:嵌套循环 / [`JoinStrategy::Hash`] / [`JoinStrategy::IndexNestedLoop`],
//!   由绑定器按等值键挑(索引嵌套循环要求内侧那列**目录里有索引**);
//! * **表达式层的短路**:`CASE` 的分支与 `AND`/`OR` 的右侧是自足子程序,只有被
//!   选中时才求值(死分支里的除零不再冒出来,见 [`Step`]).
//!
//! 语句层还认这些:`BEGIN`/`COMMIT`/`ROLLBACK`(**真事务**,撤销记录在
//! [`SymbolTable`] 里;没有显式 `BEGIN` 时引擎给每条语句一个隐式事务,
//! 成功即提交,失败即回滚),`CREATE SCHEMA`,`SET search_path`(真生效),
//! `DROP SCHEMA ... CASCADE`,`ALTER DATABASE ... SET search_path`(库级默认值),
//! 以及角色与授权那几条:`CREATE USER`/`CREATE ROLE`/`CREATE GROUP`,
//! `ALTER USER`/`ALTER ROLE`(`RENAME TO` 也在内),`DROP USER`/`DROP ROLE`,
//! `GRANT`/`REVOKE <角色>`,`GRANT`/`REVOKE <权限>`(表级 / 列级 / 库上的
//! `CONNECT`),`DROP OWNED BY`,`ALTER DATABASE ... OWNER TO`,
//! `SET ROLE`/`RESET ROLE`(当前角色是**会话状态**,记在 [`SymbolTable`] 里,
//! 所以能回滚).这些都走 [`Server`] 那个口子--语句层只把名字与取值交出去,
//! `users.json` / `grants.json` 与权限判定都在实现方.
//!
//! `VACUUM` / `ANALYZE` 也收下,但**是空转**:本版没有统计信息(基数估算与代价模型
//! 是 3.4 的待办),也没有要回收的空间,所以所有变体(含 `(ANALYZE)`,`FULL` 那类选项,
//! 多表列表)都只做一件事--把点名的表认一遍,认不出来照实报错.口径见
//! [`Stmt::Vacuum`](stmt::Stmt::Vacuum) 与 `irgen` 的 `lower_vacuum`.
//!
//! 权限检查也在语句层这一处([`Stmt`] 的每个分支动数据之前):`SELECT` 按
//! [`Plan::used_columns`](plan.rs) 给出的**列**要 `SELECT`,`INSERT` / `UPDATE` 按
//! 写到与读到的列要 `INSERT` / `UPDATE` / `SELECT`,动结构要 `check_ddl`.
//!
//! 未实现:**代价估算与连接重排**(策略是定规则挑的);CTE;`INTERSECT`/`EXCEPT`;
//! `DISTINCT ON`;窗口函数;`RIGHT`/`FULL` JOIN;`LATERAL`;`DELETE`/`DROP TABLE`/
//! `ALTER TABLE`;`SAVEPOINT` 与嵌套事务(`BEGIN` 套 `BEGIN` 只给警告);唯一索引与
//! 部分索引;`GRANT` 的 `ADMIN OPTION` / `TO PUBLIC`,`CREATE ROLE` 的 `VALID UNTIL`
//! 那一档选项;`REASSIGN OWNED BY`.(`CHECK` 与索引**不落盘**--它们不是列定义;`DEFAULT` 是列定义的一部分,
//! 跟着模式文件走.)

pub mod checks;
pub mod exec;
pub mod expr;
mod opt;
pub mod irgen;
pub mod plan;
pub mod stmt;
pub mod symbol;
pub mod value;

pub use checks::Check;
pub use exec::{execute, execute_source};
pub use expr::{BinOp, EvalError, Prog, ProgBuilder, Reg, Step, UnOp};
pub use plan::{
    Analysis, EqKey, IndexPath, JoinKind, JoinStrategy, Node, NodeId, Op, Plan, PlanBuilder, SortKey,
};
pub use stmt::{
    execute_stmt, execute_stmt_with, has_top_level_terminator, run_script, AlterRoleRequest,
    CreateRoleRequest, GrantRequest, GrantRoleRequest, GrantTarget, NoServer, Outcome,
    PrivilegeGrant, Script, Server, Stmt, StmtResult, TxOp,
};
pub use symbol::{Symbol, SymbolTable};
pub use value::{Scalar, ScalarType, Tri};

use crate::db::table::Result;

/// 把一段 SQL 解析并降级成线性 IR.
///
/// 这是给外部用的入口(`main.rs` 之类的调用方);需要一步步来的话用
/// [`irgen::lower`] 与 [`irgen::lower_statement`].
///
/// 降级本身**不执行**任何东西,也不会碰数据--要拿结果得再调 [`execute`].
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
    irgen::lower(sql, symbols)
}
