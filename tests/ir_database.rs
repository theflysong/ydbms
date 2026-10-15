//! 库与角色那几条语句在**语句层**的 testbench.
//!
//! 用一个"只在内存里记名字"的假服务目录证明两件事:
//!
//! * 语句层对文件系统**一无所知**--它只把名字(与 `search_path` 的取值,或者
//!   角色名与口令)交给 [`Server`];
//! * 没有服务目录时(`ydbms` 那个实验终端,绝大多数测试)这几条语句**明确拒绝**,
//!   不是静默无操作;表/Schema 的**权限检查**则一律放行(内存里没有用户).
//!
//! 真的建目录,写 `catalog.json` / `users.json` 在 `tests/usr_service.rs` 与
//! `tests/usr_roles.rs`,跨进程在 `tests/ysql_cli.rs`.

use ydbms::db::table::{DbError, Result};
use ydbms::ir::{
    run_script, AlterRoleRequest, CreateRoleRequest, GrantRoleRequest, Script, Server, SymbolTable,
};

/// 假的服务目录:记下被建 / 被删 / 被改的名字,以及角色那几条的入参.
#[derive(Debug, Default)]
struct FakeDbs {
    created: Vec<String>,
    dropped: Vec<(String, bool)>,
    settings: Vec<(String, Vec<String>)>,
    roles: Vec<CreateRoleRequest>,
    altered: Vec<AlterRoleRequest>,
    dropped_roles: Vec<(String, bool)>,
    memberships: Vec<GrantRoleRequest>,
    /// 记下"谁做了什么",用来证明权限判定确实被问过.
    checks: Vec<String>,
}

impl Server for FakeDbs {
    fn create_database(&mut self, _role: &str, name: &str) -> Result<String> {
        self.created.push(name.to_string());
        Ok(format!("假服务目录:库 {} 已建", name))
    }

    fn drop_database(&mut self, _role: &str, name: &str, if_exists: bool) -> Result<String> {
        self.dropped.push((name.to_string(), if_exists));
        Ok(format!("假服务目录:库 {} 已删", name))
    }

    fn set_database_search_path(
        &mut self,
        _role: &str,
        name: &str,
        path: &[String],
    ) -> Result<String> {
        self.settings.push((name.to_string(), path.to_vec()));
        Ok(format!("假服务目录:库 {} 的 search_path 已记下", name))
    }

    fn create_role(&mut self, _role: &str, request: &CreateRoleRequest) -> Result<String> {
        self.roles.push(request.clone());
        Ok(format!("假服务目录:角色 {} 已建", request.name))
    }

    fn alter_role(&mut self, _role: &str, request: &AlterRoleRequest) -> Result<String> {
        self.altered.push(request.clone());
        Ok(format!("假服务目录:角色 {} 已改", request.name))
    }

    fn drop_role(
        &mut self,
        _role: &str,
        _what: &'static str,
        names: &[String],
        if_exists: bool,
    ) -> Result<String> {
        for name in names {
            self.dropped_roles.push((name.clone(), if_exists));
        }
        Ok(format!("假服务目录:角色 {} 已删", names.join(",")))
    }

    fn grant_role(&mut self, _role: &str, request: &GrantRoleRequest) -> Result<String> {
        self.memberships.push(request.clone());
        Ok(format!(
            "假服务目录:{} {} → {}",
            match request.is_grant {
                true => "授",
                false => "收",
            },
            request.roles.join(","),
            request.members.join(",")
        ))
    }

    fn resolve_role(&mut self, current: &str, target: Option<&str>) -> Result<String> {
        Ok(target.unwrap_or(current).to_string())
    }

    fn check_ddl(&mut self, role: &str, what: &str) -> Result<()> {
        self.checks.push(format!("ddl:{role}:{what}"));
        Ok(())
    }

    fn check_create_database(&mut self, role: &str) -> Result<()> {
        self.checks.push(format!("createdb:{role}"));
        Ok(())
    }
}

/// 逐条跑一段 SQL,`CREATE/DROP DATABASE` 交给 `dbs`.
fn run_with(sql: &str, dbs: &mut dyn Server) -> Vec<String> {
    let mut symbols = SymbolTable::new();
    let script = Script::parse(sql).expect("解析");
    (0..script.len())
        .map(|index| {
            let result = script.run_one_with(index, &mut symbols, dbs);
            match result.outcome {
                Ok(ydbms::ir::Outcome::NoOp { message }) => message,
                other => panic!("第 {} 条不是 NoOp:{:?}", index + 1, other),
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
            other => panic!("{sql} 应当被显式拒绝,实际是 {other:?}"),
        }
    }

    assert!(dbs.created.is_empty() && dbs.dropped.is_empty(), "拒绝的语句不该碰服务目录");
}

#[test]
fn a_memory_script_refuses_database_statements() {
    // `run_script` 走的是 NoServer:没有服务目录就明确拒绝
    for (sql, what) in [
        ("CREATE DATABASE filmdb;", "CREATE DATABASE"),
        ("ALTER DATABASE filmdb SET search_path TO study;", "ALTER DATABASE"),
    ] {
        let mut symbols = SymbolTable::new();
        let results = run_script(sql, &mut symbols).expect("解析");

        assert_eq!(results.len(), 1);
        match &results[0].outcome {
            Err(DbError::Denied { message }) => {
                assert!(message.contains("没有服务目录"), "{message}");
                assert!(message.contains(what), "{message}");
            }
            other => panic!("{sql} 内存模式应当拒绝,实际是 {other:?}"),
        }
    }
}

#[test]
fn alter_database_set_hands_the_name_and_the_path_over() {
    let mut dbs = FakeDbs::default();
    let messages = run_with(
        "ALTER DATABASE filmdb SET search_path TO study, public;",
        &mut dbs,
    );

    assert_eq!(
        dbs.settings,
        vec![("filmdb".to_string(), vec!["study".to_string(), "public".to_string()])]
    );
    assert!(messages[0].contains("search_path 已记下"), "{}", messages[0]);
}

#[test]
fn alter_database_reset_clears_the_default() {
    // `RESET` / `TO DEFAULT` 都是"清掉":交出去的是一个空路径
    let mut dbs = FakeDbs::default();
    run_with(
        "ALTER DATABASE filmdb RESET search_path;\n\
         ALTER DATABASE filmdb SET search_path TO DEFAULT;",
        &mut dbs,
    );

    assert_eq!(
        dbs.settings,
        vec![
            ("filmdb".to_string(), Vec::new()),
            ("filmdb".to_string(), Vec::new()),
        ]
    );
}

#[test]
fn other_alter_database_forms_are_refused_rather_than_ignored() {
    let mut dbs = FakeDbs::default();

    for sql in [
        // 改名与换 owner 走的是别的语法节点(`RenameStmt` / `AlterOwnerStmt`),
        // 落到"不认识的语句"那条兜底上;这也是本版一贯的口径:显式拒绝
        "ALTER DATABASE d RENAME TO e;",
        "ALTER DATABASE d OWNER TO alice;",
        "ALTER DATABASE d SET work_mem = '64MB';",
        "ALTER DATABASE d SET search_path FROM CURRENT;",
    ] {
        let mut symbols = SymbolTable::new();
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut dbs);

        assert!(
            matches!(result.outcome, Err(DbError::Unsupported { .. })),
            "{sql} 应当被显式拒绝,实际是 {:?}",
            result.outcome
        );
    }

    assert!(dbs.settings.is_empty(), "拒绝的语句不该碰服务目录");
}

#[test]
fn database_statements_are_refused_inside_a_transaction() {
    // 建/删库与改库级默认值都改的是**服务目录**,而事务的撤销记录只管符号表:
    // 回滚不了一个已经改过的目录,那就不如不做(PostgreSQL 也不许这些语句进事务块)
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();

    for sql in [
        "BEGIN; CREATE DATABASE x;",
        "BEGIN; DROP DATABASE x;",
        "BEGIN; ALTER DATABASE x SET search_path TO study;",
    ] {
        let script = Script::parse(sql).expect("解析");
        script.run_one_with(0, &mut symbols, &mut dbs);

        let result = script.run_one_with(1, &mut symbols, &mut dbs);
        match result.outcome {
            Err(DbError::Denied { message }) => {
                assert!(message.contains("不能在事务里跑"), "{sql}: {message}")
            }
            other => panic!("{sql} 事务里应当被拒绝,实际是 {other:?}"),
        }

        // 收拾干净:把事务关掉,免得影响下一条
        let end = Script::parse("ROLLBACK;").expect("解析");
        end.run_one_with(0, &mut symbols, &mut dbs);
    }

    assert!(
        dbs.created.is_empty() && dbs.dropped.is_empty() && dbs.settings.is_empty(),
        "拒绝的语句不该碰服务目录"
    );
}

#[test]
fn a_table_name_that_cannot_be_a_file_name_is_refused() {
    // 表名就是磁盘上的文件名主干(见 `db::table::is_file_stem_safe`):
    // 带 `../` 的名字放过去就会写到库目录(甚至服务目录)**外面**.
    // 这条在建表时就拒绝,**内存模式也一样**--同一句 SQL 换到 `ysql` 才炸更糟.
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
            other => panic!("{sql} 应当被拒绝,实际是 {other:?}"),
        }
    }

    assert!(symbols.names().is_empty(), "被拒绝的表不该进符号表");
}
