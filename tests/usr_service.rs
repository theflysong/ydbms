//! 服务目录与库的 testbench（阶段 5.1 / 5.2 / 5.3 里阶段 6 用到的那一块）。
//!
//! 覆盖：库目录与 `catalog.json`、owner 与管理员、单写者锁、打开即载入、
//! 提交点只写脏表（含"表没了就把文件删掉"）、以及各种明确报错的口径。
//!
//! 同进程里换会话就够了（跨进程的端到端在 `tests/ysql_cli.rs`）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::cmd::{Session, Sink};
use ydbms::db::table::Value;
use ydbms::usr::{ServiceDir, User};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录（每个测试一个**服务目录**）。
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

    /// 建一个服务目录（`users.json` 之类这里不关心，直接手搓用户）。
    fn service(&self) -> ServiceDir {
        ServiceDir::resolve(Some(self.path.to_path_buf()), None).expect("服务目录")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 一个不碰磁盘的用户（认证在 `tests/usr_auth.rs` 里单独测）。
fn user(name: &str, is_admin: bool) -> User {
    User {
        name: name.to_string(),
        salt: String::new(),
        password_hash: String::new(),
        is_admin,
        created_unix: 0,
    }
}

/// 打开一个会话，两路输出都进内存。
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

    // 第一个句柄一放手，别人就能连（锁跟着句柄走，不会留下陈旧锁文件）
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
        assert!(session.messages().contains("成功 2 条，失败 0 条"), "{}", session.messages());
        assert_eq!(session.failed(), 0);
    } // 会话一放手：单写者锁释放，数据已经在盘上

    let lib = dir.path().join("filmdb");
    assert!(lib.join("movies.csv").is_file());
    assert!(lib.join("movies.schema.json").is_file());

    // 另一个会话（另一个进程也是同一条路）把它读回来
    let mut session = open_session(&service, "filmdb", &bob);
    assert_eq!(session.database(), Some("filmdb"));
    assert_eq!(session.user(), Some("bob"));

    let table = session.symbols().relation("movies").expect("表回来了");
    assert_eq!(table.nrows(), 3);
    assert_eq!(table.row(0).unwrap()[1], Value::Text("Alien".to_string()));
    // NULL 与带反斜杠的文本必须逐值回来（这正是 pers 的数据格式契约）
    assert_eq!(table.row(1).unwrap()[1], Value::Text("a\\b".to_string()));
    assert_eq!(table.row(2).unwrap()[1], Value::Null);

    session.run_script("SELECT movie_id, title FROM movies ORDER BY movie_id");
    assert!(session.query_output().contains("1 | Alien"), "{}", session.query_output());
    assert!(session.query_output().contains("3 | NULL"), "{}", session.query_output());
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
        // 表名带 schema 前缀，文件名就是 `study.movies.csv`
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

    assert!(!lib.join("study.movies.csv").exists(), "表没了，文件也要没");
    assert!(!lib.join("study.movies.schema.json").exists());
}

#[test]
fn a_table_created_and_dropped_in_the_same_piece_is_not_an_error() {
    // 建出来又清掉、而且都在同一段里：没有东西要写，也不该让提交失败
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

    drop(session); // 放开单写者锁，才轮得到另一个会话打开
    assert!(service.open("d", &bob).expect("再打开").load_tables().unwrap().is_empty());
}

#[test]
fn a_write_is_on_disk_before_the_session_ends() {
    // "写完就落盘"的字面口径：提交点是**一次 `run_with`**，所以 `run_script` 一返回
    // 数据就已经在盘上了——不用等会话结束、更不用等进程退出（这正是
    // "INSERT 之后立刻 kill -9，重启数据还在"要的那条性质）。
    let dir = TempDir::new("durable");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);

    let mut session = open_session(&service, "d", &bob);
    session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (9);");
    assert_eq!(session.failed(), 0, "{}", session.messages());

    // 会话还开着（锁也还握着）：直接读盘上的那份 CSV
    let csv = std::fs::read_to_string(dir.path().join("d/t.csv")).expect("t.csv 应当已经在盘上");
    assert!(csv.lines().any(|line| line == "9"), "{csv}");

    let schema = std::fs::read_to_string(dir.path().join("d/t.schema.json")).unwrap();
    assert!(schema.contains("\"name\": \"t\""), "{schema}");
}

#[test]
fn a_late_search_path_change_does_not_make_commit_save_the_wrong_table() {
    // 提交点必须按**表自己的生效名**精确取表：`s.t` 与 `t` 都在、`search_path`
    // 又指向 `s` 时，按 `search_path` 再解一次会把 `s.t` 存成 `t` 的那一份，
    // 而真正的 `t` 悄悄不落盘（踩过的坑：数据静默丢失）。
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

        // `search_path` 指向 s 之后，非限定的 `t` 指的是 `s.t`：报的是**生效名**
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

    // 正常情况：不是 owner 就不给删
    session.run_script("DROP DATABASE bobs;");
    assert!(session.messages().contains("不能删"), "{}", session.messages());
    assert!(service.exists("bobs"));

    // `catalog.json` 被改坏：**读不出来就报错**，不能当成"没有 owner 限制"放行
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

    // 手工把 a 的两个文件复制成 b：b.csv 的模式里写的还是表名 a
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
        session.messages().contains("库 filmdb 已建（owner bob）"),
        "{}",
        session.messages()
    );
    assert!(dir.path().join("filmdb/catalog.json").is_file());
    assert_eq!(service.catalog("filmdb").unwrap().owner, "bob");

    // 重名要说出来，而且不改变任何东西
    session.run_script("CREATE DATABASE filmdb;");
    assert!(session.messages().contains("already exists"), "{}", session.messages());
    assert_eq!(session.failed(), 1);

    // IF EXISTS / 不存在的库
    session.run_script("DROP DATABASE nope;");
    assert!(session.messages().contains("unknown database"), "{}", session.messages());
    session.run_script("DROP DATABASE IF EXISTS nope;");
    assert!(session.messages().contains("不存在（IF EXISTS）"), "{}", session.messages());

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

    // 正在用的那个：明确拒绝
    session.run_script("DROP DATABASE home;");
    assert!(session.messages().contains("正在使用的库"), "{}", session.messages());
    assert!(service.exists("home"));

    // 别人正连着（同进程里另一把句柄，flock 一样互斥）
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
    // `ydbms` 那个实验终端：没有服务目录，两条语句都要**明确拒绝**，不是假装成功
    let mut session = Session::with_sinks(Sink::buffer(), Sink::buffer());
    session.run_script("CREATE DATABASE filmdb;");

    assert!(session.database().is_none());
    assert!(session.messages().contains("这个会话没有服务目录"), "{}", session.messages());
    assert_eq!(session.failed(), 1);
}
