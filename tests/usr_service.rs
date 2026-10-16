//! 服务目录与库的 testbench(阶段 5.1 / 5.2 / 5.3 里阶段 6 用到的那一块).
//!
//! 覆盖:库目录与 `catalog.json`,owner 与管理员,单写者锁,打开即载入,
//! 提交点只写脏表(含"表没了就把文件删掉"),以及各种明确报错的口径.
//!
//! 同进程里换会话就够了(跨进程的端到端在 `tests/ysql_cli.rs`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::cmd::{Session, Sink};
use ydbms::db::table::{Column, ColumnsShape, DataType, Table, Value};
use ydbms::db::PhysTab;
use ydbms::pers::{CsvPersister, Persister, TableDelta, TablePath};
use ydbms::usr::service::{Manifest, TableFootprint};
use ydbms::usr::{ServiceDir, User};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录(每个测试一个**服务目录**).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-service-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 建一个服务目录(`users.json` 之类这里不关心,直接手搓用户).
    fn service(&self) -> ServiceDir {
        ServiceDir::resolve(Some(self.path.to_path_buf()), None).expect("服务目录")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 一个不碰磁盘的用户(认证在 `tests/usr_auth.rs` 里单独测).
fn user(name: &str, is_admin: bool) -> User {
    // 手搓的角色给全套开关:这个文件考的是库目录与提交点,权限判定在
    // `tests/usr_roles.rs` 与 `tests/usr_share.rs` 里单独测.
    User {
        name: name.to_string(),
        salt: String::new(),
        password_hash: String::new(),
        is_admin,
        created_unix: 0,
        can_login: true,
        can_create_db: true,
        can_create_role: true,
        inherit: true,
        member_of: Vec::new(),
    }
}

/// 打开一个会话,两路输出都进内存.
fn open_session(service: &ServiceDir, database: &str, as_user: &User) -> Session {
    let db = service.open(database, as_user).expect("打开库");
    Session::open_with_sinks(
        db,
        as_user.clone(),
        service.clone(),
        Sink::buffer(),
        Sink::buffer(),
    )
    .expect("起会话")
}

// ------------------------------------------------------------ 库目录与元数据

#[test]
fn create_database_makes_a_directory_with_a_catalog_owned_by_the_user() {
    let dir = TempDir::new("create");
    let service = dir.service();

    let message = service.create_database("filmdb", "bob").expect("建库");
    assert!(message.contains("filmdb") && message.contains("bob"), "{message}");

    let lib = dir.path().join("filmdb");
    assert!(lib.is_dir(), "库就是一个目录");

    let catalog = service.catalog("filmdb").expect("读元数据");
    assert_eq!(catalog.owner, "bob");
    assert_eq!(catalog.name, "filmdb");
    assert_eq!(catalog.version, 1);
    assert!(catalog.created_unix > 0);

    // 库自己的元数据不跟表文件混在一起
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(lib.join("catalog.json")).unwrap()).unwrap();
    assert_eq!(json["owner"].as_str(), Some("bob"));

    assert_eq!(service.databases().expect("列库"), vec!["filmdb".to_string()]);
    assert!(service.exists("filmdb"));
}

#[test]
fn creating_the_same_database_twice_is_refused() {
    let dir = TempDir::new("dup");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();

    let err = service.create_database("d", "bob").expect_err("重名要拒绝");
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn a_database_name_that_escapes_the_service_directory_is_refused() {
    let dir = TempDir::new("escape");
    let service = dir.service();

    for bad in ["", ".", "..", "a/b", "..\\x"] {
        let err = service
            .create_database(bad, "bob")
            .expect_err(&format!("`{bad}` 不该被接受"));
        assert!(err.to_string().contains("denied"), "{bad}: {err}");
    }

    // 服务目录里什么都没多出来
    assert_eq!(service.databases().unwrap().len(), 0);
}

#[test]
fn opening_an_unknown_database_is_refused() {
    let dir = TempDir::new("unknown");
    let service = dir.service();

    let err = service.open("nope", &user("bob", true)).expect_err("没有的库");
    assert!(err.to_string().contains("unknown database"), "{err}");
}

#[test]
fn a_directory_without_a_catalog_is_refused_with_a_clear_message() {
    let dir = TempDir::new("nocatalog");
    let service = dir.service();
    std::fs::create_dir(dir.path().join("handmade")).unwrap();

    let err = service
        .open("handmade", &user("bob", true))
        .expect_err("没有 catalog.json 的目录不是库");
    assert!(err.to_string().contains("catalog.json"), "{err}");
}

// ------------------------------------------------------------------ 谁能连

#[test]
fn a_non_owner_cannot_open_someone_elses_database_but_an_admin_can() {
    let dir = TempDir::new("owner");
    let service = dir.service();
    service.create_database("bobs", "bob").unwrap();
    service.create_database("alices", "alice").unwrap();

    let alice = user("alice", false);
    let admin = user("root", true);

    let err = service.open("bobs", &alice).expect_err("不是 owner");
    assert!(err.to_string().contains("alice"), "{err}");
    assert!(err.to_string().contains("denied"), "{err}");

    // 自己的库照连
    assert!(service.open("alices", &alice).is_ok());
    // 管理员连谁的都是
    assert!(service.open("bobs", &admin).is_ok());
}

#[test]
fn a_second_writer_on_the_same_database_is_refused() {
    let dir = TempDir::new("lock");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let first = service.open("d", &bob).expect("第一个写者");

    let err = service.open("d", &bob).expect_err("第二个写者要拒绝");
    assert!(err.to_string().contains("单写者锁"), "{err}");

    // 第一个句柄一放手,别人就能连(锁跟着句柄走,不会留下陈旧锁文件)
    drop(first);
    assert!(service.open("d", &bob).is_ok());
}

// -------------------------------------------------------------- 数据落盘

#[test]
fn tables_written_at_commit_are_loaded_by_the_next_session() {
    let dir = TempDir::new("persist");
    let service = dir.service();
    service.create_database("filmdb", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "filmdb", &bob);
        session.run_script(
            "CREATE TABLE movies (movie_id INT, title VARCHAR(20));\n\
             INSERT INTO movies VALUES (1, 'Alien'), (2, 'a\\b'), (3, NULL);",
        );
        assert!(session.messages().contains("成功 2 条,失败 0 条"), "{}", session.messages());
        assert_eq!(session.failed(), 0);
    } // 会话一放手:单写者锁释放,数据已经在盘上

    let lib = dir.path().join("filmdb");
    assert!(lib.join("movies.csv").is_file());
    assert!(lib.join("movies.schema.json").is_file());

    // 另一个会话(另一个进程也是同一条路)把它读回来
    let mut session = open_session(&service, "filmdb", &bob);
    assert_eq!(session.database(), Some("filmdb"));
    assert_eq!(session.user(), Some("bob"));

    let table = session.symbols().relation("movies").expect("表回来了");
    assert_eq!(table.nrows(), 3);
    assert_eq!(table.row(0).unwrap()[1], Value::Text("Alien".to_string()));
    // NULL 与带反斜杠的文本必须逐值回来(这正是 pers 的数据格式契约)
    assert_eq!(table.row(1).unwrap()[1], Value::Text("a\\b".to_string()));
    assert_eq!(table.row(2).unwrap()[1], Value::Null);

    session.run_script("SELECT movie_id, title FROM movies ORDER BY movie_id");
    assert!(session.query_output().contains("1 | Alien"), "{}", session.query_output());
    assert!(session.query_output().contains("3 | NULL"), "{}", session.query_output());
}

#[test]
fn a_long_log_is_compacted_back_into_the_base_file() {
    // 日志攒过阈值(1 MiB 与基表大小里大的那个)就在**同一次提交**里整理:基表
    // 重写一遍,日志归零.不整理的话,载入那一步要重放越来越长的日志,而同一个
    // 行号的旧版本会越攒越多(TODO 7.7).
    let dir = TempDir::new("compact");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT, pad TEXT); INSERT INTO t VALUES (0, 'seed');");
        assert_eq!(session.failed(), 0, "{}", session.messages());

        // 一条事务里追加 600 行,每行 2 KB:日志一次就攒到 1 MiB 以上
        let mut script = String::from("BEGIN;\n");
        for i in 1..=600 {
            script.push_str(&format!("INSERT INTO t VALUES ({}, '{}');\n", i, "p".repeat(2000)));
        }
        script.push_str("COMMIT;");
        session.run_script(&script);
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    let manifest = Manifest::load(&lib).unwrap().expect("提交记录在");
    let footprint = manifest.tables.get("t").copied().expect("t 的位置");
    assert_eq!(footprint.base, 601, "整理之后的基表带着全部行");
    assert_eq!(footprint.log, 0, "整理之后日志归零");
    assert_eq!(
        std::fs::metadata(lib.join("t.log")).map(|meta| meta.len()).unwrap_or(0),
        0,
        "日志要截掉"
    );

    // 另一个会话把 601 行读回来(NUL 与转义那条契约照旧)
    let session = open_session(&service, "d", &bob);
    assert_eq!(session.symbols().relation("t").unwrap().nrows(), 601);
}

#[test]
fn an_update_is_persisted_too() {
    let dir = TempDir::new("update");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1), (2);");
        session.run_script("UPDATE t SET x = x + 10 WHERE x = 2;");
        assert!(session.messages().contains("更新 1 行"), "{}", session.messages());
    }

    let session = open_session(&service, "d", &bob);
    let table = session.symbols().relation("t").unwrap();
    assert_eq!(table.row(0).unwrap()[0], Value::Int(1));
    assert_eq!(table.row(1).unwrap()[0], Value::Int(12));
}

#[test]
fn a_table_dropped_by_drop_schema_cascade_has_its_files_removed() {
    let dir = TempDir::new("cascade");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let lib = dir.path().join("d");
    {
        // 表名带 schema 前缀,文件名就是 `study.movies.csv`
        let mut session = open_session(&service, "d", &bob);
        session.run_script(
            "CREATE SCHEMA study;\nSET search_path TO study;\n\
             CREATE TABLE movies (movie_id INT);\nINSERT INTO movies VALUES (1);",
        );
        assert!(lib.join("study.movies.csv").is_file(), "建表就该落盘");
    }

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("DROP SCHEMA study CASCADE;");
        assert!(session.messages().contains("schema study 已删除"), "{}", session.messages());
    }

    assert!(!lib.join("study.movies.csv").exists(), "表没了,文件也要没");
    assert!(!lib.join("study.movies.schema.json").exists());
}

#[test]
fn a_table_created_and_dropped_in_the_same_piece_is_not_an_error() {
    // 建出来又清掉,而且都在同一段里:没有东西要写,也不该让提交失败
    let dir = TempDir::new("cascade-same-piece");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "d", &bob);
    session.run_script(
        "CREATE SCHEMA s;\nSET search_path TO s;\nCREATE TABLE t (x INT);\n\
         INSERT INTO t VALUES (1);\nDROP SCHEMA s CASCADE;",
    );

    assert_eq!(session.failed(), 0, "{}", session.messages());
    assert!(!session.messages().contains("提交失败"), "{}", session.messages());

    let lib = dir.path().join("d");
    assert!(!lib.join("s.t.csv").exists(), "压根没落盘过");

    drop(session); // 放开单写者锁,才轮得到另一个会话打开
    assert!(service.open("d", &bob).expect("再打开").load_tables().unwrap().is_empty());
}

#[test]
fn a_write_is_on_disk_before_the_session_ends() {
    // "写完就落盘"的字面口径:提交点是**一次 `run_with`**,所以 `run_script` 一返回
    // 数据就已经在盘上了--不用等会话结束,更不用等进程退出(这正是
    // "INSERT 之后立刻 kill -9,重启数据还在"要的那条性质).
    let dir = TempDir::new("durable");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "d", &bob);
    session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (9);");
    assert_eq!(session.failed(), 0, "{}", session.messages());

    // 会话还开着(锁也还握着):直接读盘上的那一对文件
    //
    // 注意行在**哪儿**:`CREATE TABLE` 那一次提交把空的基表写下去了,所以 `9` 是
    // 后来追加进 `<表>.log` 的(TODO 7.7 之后提交不再重写基表).两份文件都要对得上,
    // 才算"写完就落盘".
    let csv = std::fs::read_to_string(dir.path().join("d/t.csv")).expect("t.csv 应当已经在盘上");
    assert!(csv.lines().next().is_some(), "表头要在:{csv}");
    let log = std::fs::read_to_string(dir.path().join("d/t.log")).expect("t.log 应当已经在盘上");
    assert!(log.lines().any(|line| line == "A,0,9"), "{log}");

    let schema = std::fs::read_to_string(dir.path().join("d/t.schema.json")).unwrap();
    assert!(schema.contains("\"name\": \"t\""), "{schema}");
}

#[test]
fn a_late_search_path_change_does_not_make_commit_save_the_wrong_table() {
    // 提交点必须按**表自己的生效名**精确取表:`s.t` 与 `t` 都在,`search_path`
    // 又指向 `s` 时,按 `search_path` 再解一次会把 `s.t` 存成 `t` 的那一份,
    // 而真正的 `t` 悄悄不落盘(踩过的坑:数据静默丢失).
    let dir = TempDir::new("search-path-shift");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script(
            "CREATE TABLE s.t (x INT);\nINSERT INTO s.t VALUES (100);\n\
             CREATE TABLE t (y INT);\nINSERT INTO t VALUES (7);\nSET search_path TO s;",
        );
        assert_eq!(session.failed(), 0, "{}", session.messages());

        // `search_path` 指向 s 之后,非限定的 `t` 指的是 `s.t`:报的是**生效名**
        session.run_script("INSERT INTO t VALUES (8);");
        assert!(session.messages().contains("插入 1 行到 s.t"), "{}", session.messages());
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    let lib = dir.path().join("d");
    assert!(lib.join("s.t.csv").is_file(), "s.t 没落盘");
    assert!(lib.join("t.csv").is_file(), "t 没落盘");

    let session = open_session(&service, "d", &bob);
    let s_t = session.symbols().relation("s.t").expect("s.t 回来了");
    assert_eq!(s_t.nrows(), 2);
    assert_eq!(s_t.row(1).unwrap()[0], Value::Int(8));
    let t = session.symbols().relation("t").expect("t 回来了");
    assert_eq!(t.nrows(), 1);
    assert_eq!(t.row(0).unwrap()[0], Value::Int(7));
}

#[test]
fn a_corrupt_catalog_does_not_open_the_door_for_dropping_someone_elses_database() {
    let dir = TempDir::new("corrupt-catalog");
    let service = dir.service();
    service.create_database("bobs", "bob").unwrap();
    service.create_database("alices", "alice").unwrap();
    let alice = user("alice", false);

    let mut session = open_session(&service, "alices", &alice);

    // 正常情况:不是 owner 就不给删
    session.run_script("DROP DATABASE bobs;");
    assert!(session.messages().contains("不能删"), "{}", session.messages());
    assert!(service.exists("bobs"));

    // `catalog.json` 被改坏:**读不出来就报错**,不能当成"没有 owner 限制"放行
    std::fs::write(dir.path().join("bobs/catalog.json"), "{ 这不是 JSON }\n").unwrap();
    session.run_script("DROP DATABASE bobs;");

    assert!(!session.messages().contains("已删"), "{}", session.messages());
    assert!(session.messages().contains("catalog.json"), "{}", session.messages());
    assert!(dir.path().join("bobs").is_dir(), "库目录不该被删掉");
}

#[test]
fn a_file_stem_that_does_not_match_the_schema_name_is_refused() {
    let dir = TempDir::new("stem");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE a (x INT); INSERT INTO a VALUES (1);");
    }

    // 手工把 a 的两个文件复制成 b:b.csv 的模式里写的还是表名 a
    let lib = dir.path().join("d");
    std::fs::copy(lib.join("a.csv"), lib.join("b.csv")).unwrap();
    std::fs::copy(lib.join("a.schema.json"), lib.join("b.schema.json")).unwrap();

    let err = service
        .open("d", &bob)
        .expect("目录还在")
        .load_tables()
        .expect_err("主干名与表名对不上就该报错");
    assert!(err.to_string().contains("主干名"), "{err}");
}

// ------------------------------------------------------------ 建 / 删 数据库

#[test]
fn create_and_drop_database_from_a_session_touch_the_service_directory() {
    let dir = TempDir::new("ddl");
    let service = dir.service();
    service.create_database("home", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "home", &bob);

    session.run_script("CREATE DATABASE filmdb;");
    assert!(
        session.messages().contains("库 filmdb 已建(owner bob)"),
        "{}",
        session.messages()
    );
    assert!(dir.path().join("filmdb/catalog.json").is_file());
    assert_eq!(service.catalog("filmdb").unwrap().owner, "bob");

    // 重名要说出来,而且不改变任何东西
    session.run_script("CREATE DATABASE filmdb;");
    assert!(session.messages().contains("already exists"), "{}", session.messages());
    assert_eq!(session.failed(), 1);

    // IF EXISTS / 不存在的库
    session.run_script("DROP DATABASE nope;");
    assert!(session.messages().contains("unknown database"), "{}", session.messages());
    session.run_script("DROP DATABASE IF EXISTS nope;");
    assert!(session.messages().contains("不存在(IF EXISTS)"), "{}", session.messages());

    session.run_script("DROP DATABASE filmdb;");
    assert!(session.messages().contains("库 filmdb 已删"), "{}", session.messages());
    assert!(!dir.path().join("filmdb").exists());
}

#[test]
fn dropping_the_database_in_use_or_one_someone_else_uses_is_refused() {
    let dir = TempDir::new("drop-refused");
    let service = dir.service();
    service.create_database("home", "bob").unwrap();
    service.create_database("victim", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "home", &bob);

    // 正在用的那个:明确拒绝
    session.run_script("DROP DATABASE home;");
    assert!(session.messages().contains("正在使用的库"), "{}", session.messages());
    assert!(service.exists("home"));

    // 别人正连着(同进程里另一把句柄,flock 一样互斥)
    let held = service.open("victim", &bob).expect("另一个写者");
    session.run_script("DROP DATABASE victim;");
    assert!(session.messages().contains("正被另一个进程使用"), "{}", session.messages());
    assert!(service.exists("victim"));
    drop(held);

    session.run_script("DROP DATABASE victim;");
    assert!(!service.exists("victim"));
}

#[test]
fn a_non_owner_cannot_drop_someone_elses_database() {
    let dir = TempDir::new("drop-owner");
    let service = dir.service();
    service.create_database("bobs", "bob").unwrap();
    service.create_database("alices", "alice").unwrap();
    let alice = user("alice", false);

    let mut session = open_session(&service, "alices", &alice);
    session.run_script("DROP DATABASE bobs;");

    assert!(session.messages().contains("不能删"), "{}", session.messages());
    assert!(service.exists("bobs"));
}

// ------------------------------------------------------------------ 内存模式

#[test]
fn a_memory_session_refuses_database_statements() {
    // `ydbms` 那个实验终端:没有服务目录,两条语句都要**明确拒绝**,不是假装成功
    let mut session = Session::with_sinks(Sink::buffer(), Sink::buffer());
    session.run_script("CREATE DATABASE filmdb;");

    assert!(session.database().is_none());
    assert!(session.messages().contains("这个会话没有服务目录"), "{}", session.messages());
    assert_eq!(session.failed(), 1);
}

// ------------------------------------------------------------ 事务的落盘点(4.1)

/// 一张只有一列 `x INT` 的表(造"下一次的内容"给崩溃现场用).
fn table_of(name: &str, rows: &[i32]) -> PhysTab {
    let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
    let mut table = PhysTab::new(name, &shape);
    for row in rows {
        table.insert(vec![Value::Int(*row)]).unwrap();
    }
    table
}

#[test]
fn a_transaction_writes_nothing_before_commit() {
    // 提交点是 `COMMIT`:`BEGIN; ...` 里的语句一句都不落盘
    let dir = TempDir::new("tx-disk");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&service, "d", &bob);

        // 事务外:建表当场落盘("写完就落盘")
        session.run_script("CREATE TABLE t (x INT);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        assert!(lib.join("t.csv").is_file());

        // 事务里:插行**不**落盘(基表一个字节都不动,日志也不该长大)
        session.run_script("BEGIN; INSERT INTO t VALUES (1);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        let csv = std::fs::read_to_string(lib.join("t.csv")).unwrap();
        assert!(!csv.lines().any(|line| line == "1"), "事务里不该落盘:{csv}");
        assert!(!lib.join("t.log").exists(), "事务里日志也不该动");

        // `COMMIT` 一次把改动追加进日志(TODO 7.7:基表不重写)
        session.run_script("COMMIT;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        let log = std::fs::read_to_string(lib.join("t.log")).unwrap();
        assert!(log.lines().any(|line| line.starts_with("A,0,")), "{log}");
        let csv = std::fs::read_to_string(lib.join("t.csv")).unwrap();
        assert!(!csv.lines().any(|line| line == "1"), "基表没被重写:{csv}");
    }

    // 另一个会话把它读回来
    let session = open_session(&service, "d", &bob);
    assert_eq!(session.symbols().relation("t").unwrap().nrows(), 1);
}

#[test]
fn an_open_transaction_is_discarded_when_the_session_ends() {
    // 等价于断开连接时的 `ROLLBACK`(与 PostgreSQL 一致):留在内存里的改动不落盘
    let dir = TempDir::new("tx-open");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT);");
        session.run_script("BEGIN; INSERT INTO t VALUES (1);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    } // 会话一放手,事务就丢了

    let session = open_session(&service, "d", &bob);
    assert_eq!(session.symbols().relation("t").unwrap().nrows(), 0, "没提交的改动不落盘");
}

#[test]
fn rollback_after_a_transaction_reports_the_table_before_the_transaction() {
    // 磁盘模式下的整表回滚:撤销之后写回去的是事务前那一份
    let dir = TempDir::new("tx-rollback");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1);");
        session.run_script("BEGIN; INSERT INTO t VALUES (2); ROLLBACK;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    let session = open_session(&service, "d", &bob);
    let table = session.symbols().relation("t").unwrap();
    assert_eq!(table.nrows(), 1);
    assert_eq!(table.row(0).unwrap()[0], Value::Int(1));
}

// ------------------------------------------------- 提交的原子性与崩溃恢复(4.2)

/// 造一次"提交崩在提交点**之前**"的现场:`t` 的改动已经追加进日志,而提交记录
/// (每张表写到了哪儿)还没写.
fn crash_before_the_manifest(dir: &Path) {
    let persister = CsvPersister::new();
    let at = TablePath::new(dir, "t");

    // 把第 0 行改成 2(与 `UPDATE t SET x = 2` 等价的那一条日志)
    let pending = table_of("t", &[2]);
    let delta = TableDelta {
        appended_from: None,
        updated: [0usize].into_iter().collect(),
    };
    let committed = Manifest::load_or_empty(dir)
        .unwrap()
        .tables
        .get("t")
        .map_or(0, |at| at.log);
    persister.append_delta(&pending, &at, &delta, committed).unwrap();
    // 提交记录**没写**:日志尾巴不算数
}

/// 造一次"提交崩在提交点**之后**"的现场:提交记录已经写了(所以这次提交必须做完),
/// 但推广那一半只做了一张表.
fn crash_after_the_manifest(dir: &Path, promote_t: bool) {
    let persister = CsvPersister::new();
    let pending_t = table_of("t", &[2]);
    let pending_u = table_of("u", &[7]);

    persister
        .stage(&pending_t, &TablePath::new(dir, "t"))
        .expect("暂存 t");
    persister
        .stage(&pending_u, &TablePath::new(dir, "u"))
        .expect("暂存 u");

    let mut manifest = Manifest::load_or_empty(dir).unwrap();
    manifest.tables.insert(
        "t".to_string(),
        TableFootprint { base: 1, log: 0 },
    );
    manifest.tables.insert(
        "u".to_string(),
        TableFootprint { base: 1, log: 0 },
    );
    manifest.promoted = vec!["t".to_string(), "u".to_string()];
    manifest.save(dir).expect("写提交记录");

    if promote_t {
        // 只推广了 `t`(`save` = 暂存 + 推广,结果一样:`t` 的 `.new` 没了)
        persister
            .save(&pending_t, &TablePath::new(dir, "t"))
            .expect("推广 t");
    }
}

/// 一个已经提交过的两行库:`t` 有 1,`u` 有 7.
fn crash_fixture(tag: &str) -> (TempDir, ServiceDir, User) {
    let dir = TempDir::new(tag);
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "d", &bob);
    session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1);");
    session.run_script("CREATE TABLE u (x INT); INSERT INTO u VALUES (7);");
    assert_eq!(session.failed(), 0, "{}", session.messages());
    drop(session);

    (dir, service, bob)
}

#[test]
fn a_crash_before_the_manifest_is_all_before_the_commit() {
    let (dir, service, bob) = crash_fixture("crash-log");
    let lib = dir.path().join("d");

    // 改动已经追加进日志,提交记录还没写:那份改动**不算数**
    crash_before_the_manifest(&lib);
    assert!(lib.join("t.log").is_file(), "现场:日志在");

    // 打开库:日志尾巴被截掉,`t` 还是提交前那一份
    let session = open_session(&service, "d", &bob);
    assert_eq!(session.symbols().relation("t").unwrap().nrows(), 1);
    assert_eq!(
        session.symbols().relation("t").unwrap().row(0).unwrap()[0],
        Value::Int(1),
        "崩在提交点之前 = 旧数据"
    );
    let log = std::fs::read_to_string(lib.join("t.log")).unwrap();
    assert!(log.contains("A,0,1"), "已提交的那条还在:{log}");
    assert!(!log.contains("U,0,2"), "没越过提交点的那条要截掉:{log}");
}

#[test]
fn a_crash_after_the_manifest_keeps_the_appended_rows() {
    let (dir, service, bob) = crash_fixture("crash-manifest");
    let lib = dir.path().join("d");

    // 改动追加进日志 + 提交记录也写了:这份改动**已经提交**
    crash_before_the_manifest(&lib);
    let mut manifest = Manifest::load_or_empty(&lib).unwrap();
    let entry = manifest.tables.entry("t".to_string()).or_insert(TableFootprint {
        base: 0,
        log: 0,
    });
    entry.log += 1;
    manifest.save(&lib).unwrap();

    let session = open_session(&service, "d", &bob);
    let t = session.symbols().relation("t").unwrap();
    assert_eq!(t.nrows(), 1);
    assert_eq!(t.row(0).unwrap()[0], Value::Int(2), "崩在提交点之后 = 新数据");
    assert_eq!(
        session.symbols().relation("u").unwrap().row(0).unwrap()[0],
        Value::Int(7),
        "没被这次提交碰过的表照旧"
    );
}

#[test]
fn a_crash_in_the_middle_of_promoting_still_ends_after_the_commit() {
    let (dir, service, bob) = crash_fixture("crash-promote");
    let lib = dir.path().join("d");

    // 提交记录写了,推广只做了一半(`t` 搬了,`u` 还没)
    crash_after_the_manifest(&lib, true);
    assert!(!lib.join("t.csv.new").exists(), "t 已经搬过了");
    assert!(lib.join("u.csv.new").is_file(), "u 还没搬");

    let session = open_session(&service, "d", &bob);

    assert_eq!(
        session.symbols().relation("t").unwrap().row(0).unwrap()[0],
        Value::Int(2)
    );
    assert_eq!(
        session.symbols().relation("u").unwrap().row(0).unwrap()[0],
        Value::Int(7),
        "u 补成下一次的内容"
    );
    assert!(!lib.join("u.csv.new").exists(), "补完要把剩下的搬完");
    // 现场落定之后清单里不再有"没做完"的记号
    let manifest = Manifest::load(&lib).unwrap().expect("提交记录在");
    assert!(manifest.settled(), "补完要把 promoted / removed 清掉");
}

#[test]
fn a_broken_manifest_refuses_to_open_the_database() {
    // 提交记录坏了:宁可开不了库,也不要静默地当成"没发生过上一次提交"
    let (dir, service, bob) = crash_fixture("crash-broken");
    let lib = dir.path().join("d");

    let original = std::fs::read_to_string(lib.join("manifest.json")).unwrap();
    std::fs::write(lib.join("manifest.json"), "{ 这不是 JSON").unwrap();
    let err = service.open("d", &bob).expect_err("损坏的提交记录要挡住打开");
    assert!(format!("{err}").contains("manifest.json"), "{err}");

    // 版本不认识也一样
    std::fs::write(
        lib.join("manifest.json"),
        r#"{"version": 99, "tables": {}, "promoted": [], "removed": []}"#,
    )
    .unwrap();
    let err = service.open("d", &bob).expect_err("不认识的版本要挡住打开");
    assert!(format!("{err}").contains("99"), "{err}");

    // 修好(把原来那份写回去)之后照常打开
    //
    // 注意这里**不能**只删掉清单:那张表的行现在住在 `<表>.log` 里,而"日志里有几条
    // 算数"只有提交记录说了算--删掉它等于删掉那个账本(旧协议里基表才是账本,所以
    // 那时删掉 `commit.json` 不会丢东西).
    std::fs::write(lib.join("manifest.json"), &original).unwrap();
    let session = open_session(&service, "d", &bob);
    let t = session.symbols().relation("t").unwrap();
    assert_eq!(t.nrows(), 1);
    assert_eq!(t.row(0).unwrap()[0], Value::Int(1));
}

// ------------------------------------------------- 库级 search_path(5.4)

#[test]
fn a_database_level_search_path_applies_to_later_connections_only() {
    let dir = TempDir::new("db-settings");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE SCHEMA study; CREATE TABLE study.movies (id INT);");
        session.run_script("INSERT INTO study.movies VALUES (1);");
        session.run_script("ALTER DATABASE d SET search_path TO study;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        assert!(session.messages().contains("只影响以后的连接"), "{}", session.messages());

        // 改的是"以后的连接":**这个会话**的 search_path 一个字都没变
        session.run_script("SELECT * FROM movies;");
        assert_eq!(session.failed(), 1, "本会话不该因此解析得到 movies:{}", session.messages());
    }

    // 新连接:非限定名按库级默认值解析到 `study.movies`
    let mut session = open_session(&service, "d", &bob);
    session.run_script("SELECT id FROM movies;");
    assert_eq!(session.failed(), 0, "{}", session.messages());
    assert!(session.query_output().contains('1'), "{}", session.query_output());

    // 落盘的是 `catalog.json` 里的 `settings`
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("d/catalog.json")).unwrap())
            .unwrap();
    assert_eq!(json["settings"]["search_path"][0].as_str(), Some("study"));
}

#[test]
fn resetting_the_database_level_search_path_clears_it() {
    let dir = TempDir::new("db-settings-reset");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE SCHEMA study; ALTER DATABASE d SET search_path TO study;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        session.run_script("ALTER DATABASE d SET search_path TO DEFAULT;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    let mut session = open_session(&service, "d", &bob);
    session.run_script("SELECT * FROM movies;");
    assert_eq!(session.failed(), 1, "清掉之后非限定名又解析不到了");
}

#[test]
fn a_non_owner_cannot_change_someone_elses_database_settings() {
    let dir = TempDir::new("db-settings-owner");
    let service = dir.service();
    service.create_database("bobs", "bob").unwrap();
    service.create_database("alices", "alice").unwrap();

    let alice = user("alice", false);
    let mut session = open_session(&service, "alices", &alice);
    session.run_script("ALTER DATABASE bobs SET search_path TO study;");
    assert!(session.messages().contains("不能改"), "{}", session.messages());
    assert!(service.catalog("bobs").unwrap().settings.search_path.is_empty());
    drop(session); // 放开单写者锁,才轮得到下一个会话

    // 管理员可以
    let admin = user("root", true);
    let mut session = open_session(&service, "alices", &admin);
    session.run_script("ALTER DATABASE bobs SET search_path TO study;");
    assert_eq!(session.failed(), 0, "{}", session.messages());
    assert_eq!(service.catalog("bobs").unwrap().settings.search_path, ["study"]);
}

#[test]
fn an_old_catalog_without_settings_still_opens() {
    // `settings` 是后加的字段:野外的老 `catalog.json` 必须照旧读得回来
    let dir = TempDir::new("old-catalog");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    std::fs::write(
        dir.path().join("d/catalog.json"),
        "{\"version\": 1, \"name\": \"d\", \"owner\": \"bob\", \"created_unix\": 1}\n",
    )
    .unwrap();

    let session = open_session(&service, "d", &bob);
    assert!(session.symbols().search_path().is_empty());
}

#[test]
fn a_database_level_setting_cannot_be_sneaked_in_inside_a_transaction() {
    // 它写的是 `catalog.json`(服务目录),而事务撤销不了目录--所以事务里**明确
    // 拒绝**,而不是让它静默生效(那正是"回滚不生效"最坏的一种)
    let dir = TempDir::new("db-settings-tx");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "d", &bob);
    session.run_script("BEGIN; ALTER DATABASE d SET search_path TO study; ROLLBACK;");

    assert!(session.messages().contains("不能在事务里跑"), "{}", session.messages());
    assert!(service.catalog("d").unwrap().settings.search_path.is_empty(), "一个字都不该写进去");
}

/// 把库目录设成只读.返回 `false` 表示这个环境拦不住写(比如以 root 跑测试),
/// 相关测试应当直接跳过--它们靠"目录不可写"来证明"那条路根本不碰磁盘".
#[cfg(unix)]
fn make_read_only(lib: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    if std::fs::write(lib.join(".probe"), b"x").is_ok() {
        let _ = std::fs::remove_file(lib.join(".probe"));
        return false;
    }

    let mut perms = std::fs::metadata(lib).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(lib, perms).unwrap();
    true
}

#[cfg(unix)]
fn make_writable(lib: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(lib).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(lib, perms).unwrap();
}

#[test]
#[cfg(unix)]
fn a_read_only_statement_does_not_touch_the_database_directory() {
    // 只读语句一个字节的 IO 都不做:否则库目录只读时,一次纯 `SELECT` 会变成
    // "查询成功,却报提交失败"
    let dir = TempDir::new("read-only-select");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1);");
        assert_eq!(session.failed(), 0, "{}", session.messages());

        if !make_read_only(&lib) {
            return; // 权限拦不住,这条测试证明不了什么
        }

        session.run_script("SELECT x FROM t;");
        assert_eq!(session.failed(), 0, "只读查询不该报错:{}", session.messages());
        assert!(!session.messages().contains("提交失败"), "{}", session.messages());

        make_writable(&lib);
    }
}

#[test]
#[cfg(unix)]
fn a_warned_rollback_keeps_a_pending_retry() {
    // 提交失败时 `dirty` 被有意保留(下次再试).此时敲一条**没有事务**的
    // `ROLLBACK`(按 PostgreSQL 只发一句警告,什么都没撤销),那批改动必须还留着
    // --丢掉它就等于"报了错,还悄悄不再落盘".
    let dir = TempDir::new("retry-after-warning");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1);");
        assert_eq!(session.failed(), 0, "{}", session.messages());

        if !make_read_only(&lib) {
            return; // 拦不住写,这条测试证明不了什么
        }

        // 这一条写进内存了,但落盘失败 ⇒ `dirty` 留着等下次重试
        session.run_script("INSERT INTO t VALUES (2);");
        assert!(session.messages().contains("提交失败"), "{}", session.messages());

        // 只发警告的 `ROLLBACK`:什么都撤销不了,也不该把重试集抹掉
        session.run_script("ROLLBACK;");
        assert!(session.messages().contains("警告"), "{}", session.messages());

        make_writable(&lib);

        // 下一条语句的自提交把欠着的那张表补写回去
        session.run_script("SELECT x FROM t;");
        assert_eq!(session.failed(), 1, "只有那次失败:{}", session.messages());
    }

    // 另一个会话读回来:两行都在(欠的那一次提交补上了)
    let session = open_session(&service, "d", &bob);
    assert_eq!(session.symbols().relation("t").unwrap().nrows(), 2, "重试集不该被警告抹掉");
}
