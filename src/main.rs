//! `ydbms` 的命令行入口——**内存模式**的实验终端。
//!
//! 这一层**只负责把控制权交出去**：交互式终端（读一行、Tab 补全、多行累积）、
//! 执行引擎、两路输出都在 [`ydbms::cmd`] 里。放这儿的东西越少越好——`main.rs`
//! 里的状态是拿不到的那些（脚本、测试）。
//!
//! # 两个二进制
//!
//! * `ydbms`（这个文件）—— **内存模式**：一个空库，退出即丢。适合试 SQL 的手感。
//! * `ysql`（[`src/bin/ysql.rs`]）—— **服务模式**：用户认证 + 磁盘上的库，
//!   入口是 [`ydbms::cmd::cli::run`](ydbms::cmd::cli::run)。
//!
//! # `ydbms` 的用法
//!
//! ```text
//! sql> \i tests/script/db1/db.sql
//! sql> \dt
//! sql> SELECT value, square FROM prime WHERE value > 50
//! sql> EXPLAIN SELECT * FROM prime ORDER BY prime_id
//! sql> \timing on
//! sql> \q
//! ```
//!
//! # `ysql` 的用法
//!
//! ```text
//! ysql [-U 用户] [-d 库] [-f 脚本] [--server-dist 目录] [-h]
//! ```
//!
//! * `-U` 默认 `$USER`，`-d` 默认与 `-U` 同名，`--server-dist` 默认 `$HOME/.ydbms`；
//! * 口令从 `PGPASSWORD` 来（没有它时交互式会问，非交互直接报错）；
//! * 没有 `-f` 就进交互式终端（`ysql < 脚本.sql` 也从标准输入读）；
//! * `-f <脚本>` 与交互式里的 `\i <脚本>` 走的是同一条路，输出逐字一致；
//! * 退出码：0 正常；1 用法 / 认证 / 打开库失败，或 `-f` 的脚本里报过错。
//!
//! ```text
//! export PGPASSWORD=secret
//! ysql -U bob --server-dist ~/.ydbms -f create.sql   # 首次使用：bob 建为管理员
//! ysql -U bob -d filmdb --server-dist ~/.ydbms       # 交互式
//! ```
//!
//! # 服务目录长什么样
//!
//! ```text
//! ~/.ydbms/                           --server-dist 指向的目录，首次使用自己建
//! ├── users.json                      用户表：加盐哈希，没有明文
//! ├── bob/                            一个目录 = 一个库（首次连 -U bob 时建出来）
//! │   ├── catalog.json                库的元数据：owner、创建时间、格式版本
//! │   ├── .lock                       单写者锁：第二个连上来的进程直接报错
//! │   ├── movies.csv                  表数据（表名就是文件名主干）
//! │   └── movies.schema.json          表模式
//! └── filmdb/
//!     └── …（同上）
//! ```
//!
//! 写是**提交即落盘**：一次提交 = 敲进来的一段、或 `-f`/`\i` 读进来的一个文件，
//! 提交点只写脏表。**没有真事务**（`BEGIN`/`COMMIT` 只记状态），所以报错不回滚前面
//! 已经执行的语句——详见 [`ydbms::cmd::engine`] 的文件头。

fn main() {
    if let Err(e) = ydbms::cmd::repl::run() {
        eprintln!("错误：{e}");
        std::process::exit(1);
    }
}
