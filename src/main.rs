//! `ydbms` 的命令行入口。
//!
//! 这一层**只负责把控制权交出去**：交互式终端（读一行、Tab 补全、多行累积）、
//! 执行引擎、两路输出都在 [`ydbms::cmd`] 里。放这儿的东西越少越好——`main.rs`
//! 里的状态是拿不到的那些（脚本、测试）。
//!
//! 用法：
//!
//! ```text
//! sql> \i tests/script/db1/db.sql
//! sql> \dt
//! sql> SELECT value, square FROM prime WHERE value > 50
//! sql> EXPLAIN SELECT * FROM prime ORDER BY prime_id
//! sql> \timing on
//! sql> \q
//! ```

fn main() {
    if let Err(e) = ydbms::cmd::repl::run() {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}
