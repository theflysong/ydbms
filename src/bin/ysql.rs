//! `ysql` -- 带用户与库的命令行入口(ydbms 的服务模式).
//!
//! 真正的活都在 [`ydbms::cmd::cli`] 里;这个文件只把它的退出码交给 shell.
//! 用法,参数默认值与服务目录树见 `src/main.rs` 的文件头.

fn main() {
    std::process::exit(ydbms::cmd::cli::run());
}
