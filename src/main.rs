//! `ydbms` 的命令行入口--**内存模式**的实验终端.
//!
//! 这一层**只负责把控制权交出去**:交互式终端(读一行,Tab 补全,多行累积),
//! 执行引擎,两路输出都在 [`ydbms::cmd`] 里.放这儿的东西越少越好--`main.rs`
//! 里的状态是拿不到的那些(脚本,测试).
//!
//! # 两个二进制
//!
//! * `ydbms`(这个文件)-- **内存模式**:一个空库,退出即丢.适合试 SQL 的手感.
//! * `ysql`([`src/bin/ysql.rs`])-- **服务模式**:用户认证 + 磁盘上的库,
//!   入口是 [`ydbms::cmd::cli::run`](ydbms::cmd::cli::run).
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
//! * `-U` 默认 `$USER`,`-d` 默认与 `-U` 同名,`--server-dist` 默认 `$HOME/.ydbms`;
//!   相对路径按**启动时**的工作目录定死,之后的 `\cd` 改不动它;
//! * 口令从 `PGPASSWORD` 来(没有它时交互式会问,非交互直接报错);
//! * 没有 `-f` 就进交互式终端(`ysql < 脚本.sql` 也从标准输入读);
//! * `-f <脚本>` 与交互式里的 `\i <脚本>` 走的是同一条执行路径,但 `-f` 是**脚本
//!   模式**:每条语句的状态行与汇总默认不打(`\set VERBOSE on` 能打开),`\i` 那边
//!   默认打;
//! * 退出码:0 正常;1 用法 / 认证 / 打开库失败,或 `-f` 的脚本里报过错.
//!
//! ```text
//! export PGPASSWORD=secret
//! ysql -U bob --server-dist ~/.ydbms -f create.sql   # 首次使用:bob 建为管理员
//! ysql -U bob -d filmdb --server-dist ~/.ydbms       # 交互式
//! ```
//!
//! # 服务目录长什么样
//!
//! ```text
//! ~/.ydbms/                           --server-dist 指向的目录,首次使用自己建
//! ├── users.json                      用户表:加盐哈希,没有明文
//! ├── bob/                            一个目录 = 一个库(首次连 -U bob 时建出来)
//! │   ├── catalog.json                库的元数据:owner,创建时间,库级 search_path,格式版本
//! │   ├── .lock                       单写者锁:第二个连上来的进程直接报错
//! │   ├── manifest.json               提交记录:每张表已提交到哪儿,是哪种格式,崩了靠它补完
//! │   ├── <表>.heap                   基表:页式堆文件(默认格式;表名就是文件名主干)
//! │   ├── <表>.log                    堆表:行位置日志,挪过窝的行的新地址
//! │   ├── <表>.csv                    基表:标准 CSV(`ysql-csv` / `--storage csv`)
//! │   └── movies.schema.json          表模式
//! └── filmdb/
//!     └── ...(同上)
//! ```
//!
//! 写是**提交即落盘**:没有事务时每条语句自己提交一次(失败即回滚这一句),
//! 表用哪种格式落盘由会话的存储模式定(`ysql` 默认页式堆文件,`ysql-csv` 预设标准
//! CSV,两种格式可以同库并存),提交点只写脏表,一次提交是**原子的**(追加式的堆文件
//! + 提交记录,见
//! [`ydbms::usr::service::Database::commit`]);`BEGIN; ... COMMIT;` 里则一次都不落盘,
//! `COMMIT` 才写.详见 [`ydbms::cmd::engine`] 的文件头.

// 二进制是**另一个 crate**:`tr!` 展开出来的 `rust_i18n::t!` 要落到本 crate 里那份
// `_rust_i18n_t`,所以这里也得 `i18n!` 一次(rust-i18n 本来就是这个用法:每个 crate
// 各自 init 自己的表).语言那个全局量是与库共享的.
rust_i18n::i18n!("locales", fallback = "zh-CN");

use ydbms::tr;

fn main() {
    ydbms::i18n::init_from_env();
    if let Err(e) = ydbms::cmd::repl::run() {
        eprintln!("{}", tr!("main.001", e = e));
        std::process::exit(1);
    }
}
