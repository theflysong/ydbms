//! `ydbms` —— 一个用于学习 DBMS 内部机制的实验性数据库管理系统。
//!
//! 目前提供表抽象层：
//!
//! * [`db::table`] —— [`Table`](db::table::Table) /
//!   [`TabShape`](db::table::TabShape) 等抽象定义。
//! * [`db::phys`] —— 物理表 [`PhysTab`](db::phys::PhysTab)。
//! * [`db::source`] —— 拉取式的中间结果
//!   [`RowSource`](db::source::RowSource)（与随机访问的
//!   [`Table`](db::table::Table) 并列）。
//! * [`pers`] —— 持久化：[`Persister`](pers::Persister) 抽象与
//!   [`CsvPersister`](pers::CsvPersister) 实现。
//! * [`ir`] —— 引擎里的 **SQL 部分**：线性 IR（[`Plan`](ir::Plan) / [`Prog`](ir::Prog)）、
//!   [`SymbolTable`](ir::SymbolTable)、拉取式的 [`execute`](ir::execute)、
//!   以及"解析一次、逐条执行"的 [`Script`](ir::Script)。
//! * [`usr`] —— **用户与库**：[`User`](usr::User) / [`UserStore`](usr::UserStore)（认证）
//!   与服务目录（[`ServiceDir`](usr::ServiceDir) / [`Database`](usr::Database)）。
//! * [`cmd`] —— **执行引擎**：接受任务（SQL 与元命令混在一起的文本），驱动 [`ir`]，
//!   管住"查询输出"与"消息输出"两路；交互式终端（含 Tab 补全）与 `ysql` 的命令行
//!   （[`cmd::cli`](cmd::cli)）也在这里。
//!
//! # 两个二进制
//!
//! * `ydbms` —— 内存里的实验终端：一个空库、退出即丢（[`cmd::repl::run`]）。
//! * `ysql` —— 服务模式：服务目录 + 用户认证 + 磁盘上的库（[`cmd::cli::run`]）。

pub mod cmd;
pub mod db;
pub mod ir;
pub mod pers;
pub mod usr;
