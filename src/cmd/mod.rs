//! 执行引擎:接受**任务**,驱动 SQL 部分,管住两路输出.
//!
//! 任务的来源只有三种,在引擎里没有区别:
//!
//! * 交互式终端敲进来的一段([`repl`]);
//! * `\i <文件>` 读进来的内容([`Session::run_file`])--`ysql -f <脚本>` 也走它;
//! * 元命令本身([`metacmd`],引擎自己执行).
//!
//! # 与 `ir` 的分工
//!
//! **引擎里没有 SQL 知识**:解析,降级,执行都是 [`crate::ir`] 的事.这一层只管
//! "哪一段是 SQL,哪一段是元命令,结果往哪一路写,出错要不要停".依赖方向是
//! `cmd` → `ir`,反过来不行.
//!
//! # 各文件
//!
//! - `engine.rs` -- [`Session`]:符号表,两路输出,元命令的**语义**,
//!   `\timing` / `ON_ERROR_STOP` / `VERBOSE` 三个开关,以及服务模式下的**提交点**.
//! - `metacmd.rs` -- 元命令的**语法**(只拆词,不校验).
//! - `repl.rs` -- 交互式终端:rustyline 接线,多行判定,非 tty 回退.
//! - `complete.rs` -- Tab 补全的候选(纯逻辑,不认识终端).
//! - `cli.rs` -- `ysql` 的命令行:参数解析,认证,打开库,退出码.

pub mod cli;
pub mod complete;
pub mod engine;
pub mod metacmd;
pub mod repl;

pub use complete::{Catalog, Candidate};
pub use engine::{Session, Sink, MAX_INCLUDE_DEPTH};
pub use metacmd::MetaCmd;
