//! `CREATE DATABASE` / `DROP DATABASE` 在**语句层**的 testbench。
//!
//! 用一个"只在内存里记名字"的假服务目录证明两件事：
//!
//! * 语句层对文件系统**一无所知**——它只把名字交给 [`Databases`]；
//! * 没有服务目录时（`ydbms` 那个实验终端、绝大多数测试）这两条语句**明确拒绝**，
//!   不是静默无操作。
//!
//! 真的建目录、写 `catalog.json` 在 `tests/usr_service.rs`，跨进程在 `tests/ysql_cli.rs`。

use ydbms::db::table::{DbError, Result};
use ydbms::ir::{run_script, Databases, Script, SymbolTable};

/// 假的服务目录：记下被建 / 被删的名字。
#[derive(Debug, Default)]
struct FakeDbs {
    created: Vec<String>,
    dropped: Vec<(String, bool)>,
}

impl Databases for FakeDbs {
    fn create_database(&mut self, name: &str) -> Result<String> {
        self.created.push(name.to_string());
        Ok(format!("假服务目录：库 {} 已建", name))
    }

    fn drop_database(&mut self, name: &str, if_exists: bool) -> Result<String> {
        self.dropped.push((name.to_string(), if_exists));
        Ok(format!("假服务目录：库 {} 已删", name))
    }
}

/// 逐条跑一段 SQL，`CREATE/DROP DATABASE` 交给 `dbs`。
fn run_with(sql: &str, dbs: &mut dyn Databases) -> Vec<String> {
    let mut symbols = SymbolTable::new();
    let script = Script::parse(sql).expect("解析");
    (0..script.len())
        .map(|index| {
            let result = script.run_one_with(index, &mut symbols, dbs);
            match result.outcome {
                Ok(ydbms::ir::Outcome::NoOp { message }) => message,
                other => panic!("第 {} 条不是 NoOp：{:?}", index + 1, other),
            }
        })
        .collect()
}

#[test]
fn create_and_drop_database_reach_the_capability() {
    let mut dbs = FakeDbs::default();

    let messages = run_with("CREATE DATABASE filmdb; DROP DATABASE old;", &mut dbs);

    assert_eq!(dbs.created, vec!["filmdb".to_string()]);
    assert_eq!(dbs.dropped, vec![("old".to_string(), false)]);
    assert!(messages[0].contains("filmdb 已建"), "{}", messages[0]);
    assert!(messages[1].contains("old 已删"), "{}", messages[1]);
}

#[test]
fn if_exists_reaches_the_capability() {
    let mut dbs = FakeDbs::default();
    let messages = run_with("DROP DATABASE IF EXISTS old;", &mut dbs);

    assert_eq!(dbs.dropped, vec![("old".to_string(), true)]);
    assert!(messages[0].contains("old 已删"), "{}", messages[0]);
}

#[test]
fn database_options_are_refused_rather_than_ignored() {
    let mut dbs = FakeDbs::default();

    for sql in [
        "CREATE DATABASE d ENCODING 'LATIN1';",
        "CREATE DATABASE d OWNER alice;",
        "DROP DATABASE d WITH (FORCE);",
    ] {
        let mut symbols = SymbolTable::new();
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut dbs);

        match result.outcome {
            Err(DbError::Unsupported { feature }) => {
                assert!(feature.contains(" DATABASE "), "{sql}: {feature}")
            }
            other => panic!("{sql} 应当被显式拒绝，实际是 {other:?}"),
        }
    }

    assert!(dbs.created.is_empty() && dbs.dropped.is_empty(), "拒绝的语句不该碰服务目录");
}

#[test]
fn a_memory_script_refuses_database_statements() {
    // `run_script` 走的是 NoDatabases：没有服务目录就明确拒绝
    let mut symbols = SymbolTable::new();
    let results = run_script("CREATE DATABASE filmdb;", &mut symbols).expect("解析");

    assert_eq!(results.len(), 1);
    match &results[0].outcome {
        Err(DbError::Denied { message }) => {
            assert!(message.contains("没有服务目录"), "{message}")
        }
        other => panic!("内存模式应当拒绝，实际是 {other:?}"),
    }
}

#[test]
fn a_table_name_that_cannot_be_a_file_name_is_refused() {
    // 表名就是磁盘上的文件名主干（见 `db::table::is_file_stem_safe`）：
    // 带 `../` 的名字放过去就会写到库目录（甚至服务目录）**外面**。
    // 这条在建表时就拒绝，**内存模式也一样**——同一句 SQL 换到 `ysql` 才炸更糟。
    let mut symbols = SymbolTable::new();

    for sql in [
        r#"CREATE TABLE "../evil" (a INT);"#,
        r#"CREATE TABLE "a/b" (a INT);"#,
        r#"CREATE TABLE ".." (a INT);"#,
        r#"CREATE TABLE "a\b" (a INT);"#,
    ] {
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut FakeDbs::default());

        match result.outcome {
            Err(DbError::Denied { message }) => {
                assert!(message.contains("文件名"), "{sql}: {message}")
            }
            other => panic!("{sql} 应当被拒绝，实际是 {other:?}"),
        }
    }

    assert!(symbols.names().is_empty(), "被拒绝的表不该进符号表");
}
