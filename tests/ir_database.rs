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

use ydbms::ir::NoServer;
use ydbms::ir::{
    run_script, AlterRoleRequest, CreateRoleRequest, GrantRequest, GrantRoleRequest, GrantTarget,
    PrivilegeGrant, Script, Server, SymbolTable,
};
use ydbms::usr::Privilege;

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
    /// 记下表 / 列权限检查的入参.
    table_checks: Vec<(String, String, String, Vec<String>)>,
    /// 记下对象授权与 `DROP OWNED BY` / 换 owner 的入参.
    grants: Vec<GrantRequest>,
    owned: Vec<Vec<String>>,
    owners: Vec<(String, String)>,
    forgot: Vec<String>,
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

    fn check_table(
        &mut self,
        role: &str,
        table: &str,
        want: Privilege,
        columns: &[String],
    ) -> Result<()> {
        self.table_checks.push((
            role.to_string(),
            table.to_string(),
            want.as_str().to_string(),
            columns.to_vec(),
        ));
        Ok(())
    }

    fn grant(&mut self, _role: &str, request: &GrantRequest) -> Result<String> {
        self.grants.push(request.clone());
        Ok(format!("假服务目录:{} 已记下", if request.is_grant { "GRANT" } else { "REVOKE" }))
    }

    fn drop_owned(&mut self, _role: &str, roles: &[String]) -> Result<String> {
        self.owned.push(roles.to_vec());
        Ok(format!("假服务目录:收了 {}", roles.join(",")))
    }

    fn set_database_owner(&mut self, _role: &str, name: &str, owner: &str) -> Result<String> {
        self.owners.push((name.to_string(), owner.to_string()));
        Ok(format!("假服务目录:库 {} 的 owner 换成 {}", name, owner))
    }

    fn forget_grants(&mut self, table: &str) -> Result<()> {
        self.forgot.push(table.to_string());
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
        // 改名走 `RenameStmt` 落到"不认识的语句"那条兜底上;别的变量本版没有会话
        // 默认值可言.这也是本版一贯的口径:显式拒绝,不假装.
        // (`ALTER DATABASE ... OWNER TO` 是支持的,见下面那条用例.)
        "ALTER DATABASE d RENAME TO e;",
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
fn alter_database_owner_reaches_the_capability() {
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();
    let script = Script::parse("ALTER DATABASE filmdb OWNER TO alice;").expect("解析");
    let result = script.run_one_with(0, &mut symbols, &mut dbs);

    assert!(result.outcome.is_ok(), "{:?}", result.outcome);
    assert_eq!(dbs.owners, vec![("filmdb".to_string(), "alice".to_string())]);
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

// ------------------------------------------------------------ 授权(6.4)

/// 建一张表(顺便把 `forget_grants` 那条路走一遍).
fn create_movies(dbs: &mut FakeDbs, symbols: &mut SymbolTable) {
    let script = Script::parse("CREATE TABLE movies (movie_id INT, title TEXT);").expect("解析");
    let result = script.run_one_with(0, symbols, dbs);
    assert!(result.outcome.is_ok(), "{:?}", result.outcome);
}

#[test]
fn a_create_table_clears_stale_grants_of_the_same_name() {
    // 删过的表名又被建出来时,旧授权不许悄悄生效
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();
    create_movies(&mut dbs, &mut symbols);
    assert_eq!(dbs.forgot, vec!["movies".to_string()]);
}

#[test]
fn grant_reaches_the_server_capability() {
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();
    create_movies(&mut dbs, &mut symbols);

    let script = Script::parse("GRANT SELECT, INSERT (title) ON movies TO alice, bob;").expect("解析");
    let result = script.run_one_with(0, &mut symbols, &mut dbs);
    assert!(result.outcome.is_ok(), "{:?}", result.outcome);

    assert_eq!(dbs.grants.len(), 1);
    let request = &dbs.grants[0];
    assert!(request.is_grant);
    assert_eq!(request.target, GrantTarget::Tables(vec!["movies".to_string()]));
    assert_eq!(request.grantees, vec!["alice".to_string(), "bob".to_string()]);
    assert_eq!(
        request.grants,
        vec![
            PrivilegeGrant {
                privilege: Privilege::Select,
                columns: Vec::new(),
            },
            PrivilegeGrant {
                privilege: Privilege::Insert,
                columns: vec!["title".to_string()],
            },
        ]
    );

    // `ALL` 展开成本版的三种,`REVOKE` 走的是同一个变体
    let script = Script::parse("REVOKE ALL ON movies FROM alice;").expect("解析");
    script.run_one_with(0, &mut symbols, &mut dbs);
    let request = &dbs.grants[1];
    assert!(!request.is_grant);
    assert_eq!(request.grants.len(), 3);
}

#[test]
fn connect_and_owner_and_drop_owned_reach_the_capability() {
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();

    for sql in [
        "GRANT CONNECT ON DATABASE filmdb TO alice;",
        "ALTER DATABASE filmdb OWNER TO alice;",
        "DROP OWNED BY alice, bob;",
    ] {
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut dbs);
        assert!(result.outcome.is_ok(), "{sql}: {:?}", result.outcome);
    }

    assert_eq!(dbs.grants[0].target, GrantTarget::Database("filmdb".to_string()));
    assert!(dbs.grants[0].grants.is_empty(), "库上只有 CONNECT");
    assert_eq!(dbs.owners, vec![("filmdb".to_string(), "alice".to_string())]);
    assert_eq!(dbs.owned, vec![vec!["alice".to_string(), "bob".to_string()]]);
}

#[test]
fn grant_forms_are_refused_rather_than_ignored() {
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();
    create_movies(&mut dbs, &mut symbols);

    for sql in [
        // 本版没有 DELETE
        "GRANT DELETE ON movies TO alice;",
        // 整体写法
        "GRANT SELECT ON ALL TABLES IN SCHEMA study TO alice;",
        "GRANT SELECT ON SCHEMA study TO alice;",
        // 没有 `PUBLIC` 那一档,也不认 `CURRENT_USER`
        "GRANT SELECT ON movies TO PUBLIC;",
        "GRANT SELECT ON movies TO CURRENT_USER;",
        // 没有 admin option
        "GRANT SELECT ON movies TO alice WITH GRANT OPTION;",
        "GRANT reader TO alice WITH ADMIN OPTION;",
        // 库上只认 CONNECT
        "GRANT CREATE ON DATABASE filmdb TO alice;",
        "GRANT CONNECT (title) ON DATABASE filmdb TO alice;",
    ] {
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut dbs);
        assert!(
            matches!(result.outcome, Err(DbError::Unsupported { .. })),
            "{sql} 应当被显式拒绝,实际是 {:?}",
            result.outcome
        );
    }

    // 表 / 列不存在:不是"unsupported",而是找不到
    for sql in [
        "GRANT SELECT ON nope TO alice;",
        "GRANT SELECT (nosuch) ON movies TO alice;",
    ] {
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut dbs);
        assert!(result.outcome.is_err(), "{sql} 应当报错");
        assert!(
            !matches!(result.outcome, Err(DbError::Unsupported { .. })),
            "{sql} 不该是 unsupported"
        );
    }

    assert!(dbs.grants.is_empty(), "拒绝的语句不该碰服务目录");
}

#[test]
fn a_memory_session_refuses_grant_statements() {
    let mut symbols = SymbolTable::new();
    let script = Script::parse("CREATE TABLE movies (movie_id INT);").expect("解析");
    script.run_one_with(0, &mut symbols, &mut FakeDbs::default());

    for sql in [
        "GRANT SELECT ON movies TO alice;",
        "DROP OWNED BY alice;",
        "ALTER DATABASE filmdb OWNER TO alice;",
    ] {
        let script = Script::parse(sql).expect("解析");
        let result = script.run_one_with(0, &mut symbols, &mut NoServer);
        match result.outcome {
            Err(DbError::Denied { message }) => {
                assert!(message.contains("没有服务目录"), "{sql}: {message}")
            }
            other => panic!("{sql} 内存模式应当拒绝,实际是 {other:?}"),
        }
    }
}

#[test]
fn grant_statements_are_refused_inside_a_transaction() {
    let mut dbs = FakeDbs::default();
    let mut symbols = SymbolTable::new();
    create_movies(&mut dbs, &mut symbols);

    for sql in [
        "BEGIN; GRANT SELECT ON movies TO alice;",
        "BEGIN; DROP OWNED BY alice;",
        "BEGIN; ALTER DATABASE filmdb OWNER TO alice;",
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
        let end = Script::parse("ROLLBACK;").expect("解析");
        end.run_one_with(0, &mut symbols, &mut dbs);
    }

    assert!(dbs.grants.is_empty() && dbs.owned.is_empty() && dbs.owners.is_empty());
}
