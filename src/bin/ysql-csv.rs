//! `ysql-csv` -- 与 `ysql` **同一个程序**,只是把默认的存储模式预设成标准 CSV.
//!
//! 新建的表落成 `<表>.csv`(纯列名表头 + 数据行,别的工具直接读),代价是整表读进
//! 内存,每次提交整表重写;页式堆表那份默认留给 `ysql`.两种格式可以在同一个库里
//! 并存--表自己的格式记在库目录的 `manifest.json` 里,打开库时按记录分派.
//!
//! 命令行给了 `--storage` 就以命令行为准(`ysql-csv --storage heap` 与 `ysql` 等价).
//! 参数,口令,服务目录树与退出码见 [`ydbms::cmd::cli`] 与 `src/main.rs` 的文件头.

fn main() {
    std::process::exit(ydbms::cmd::cli::run_with(ydbms::pers::Storage::Csv));
}
