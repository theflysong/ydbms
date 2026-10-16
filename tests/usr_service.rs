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
use ydbms::pers::heap::encode_row;

use ydbms::pers::Storage;
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

/// 同一个服务目录,但**存储模式预设成 CSV**(新建的表落成 `<表>.csv`).
fn csv_service(service: &ServiceDir) -> ServiceDir {
    service.clone().with_storage(Storage::Csv)
}

// ------------------------------------------------------------ 存储模式(CSV / 堆)

#[test]
fn a_csv_table_is_written_as_plain_csv_and_read_back() {
    let dir = TempDir::new("csv-mode");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&csv_service(&service), "d", &bob);
        session.run_script("CREATE TABLE t (x INT, note TEXT); INSERT INTO t VALUES (1, 'a'), (2, 'b');");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    // 盘上是**标准 CSV**:表头 + 数据行,别的工具直接读
    let text = std::fs::read_to_string(lib.join("t.csv")).expect("t.csv 该在");
    assert_eq!(text, "x,note\n1,a\n2,b\n");
    assert!(!lib.join("t.heap").exists(), "CSV 模式不该落堆文件");
    let manifest = Manifest::load(&lib).unwrap().expect("提交记录在");
    let footprint = manifest.tables.get("t").copied().expect("t 的位置");
    assert!(footprint.csv && !footprint.heap, "{footprint:?}");
    assert_eq!(footprint.csv_rows, 2);

    // 再开一次(还是 CSV 模式)读回来
    let session = open_session(&csv_service(&service), "d", &bob);
    let t = session.symbols().relation("t").expect("t 回来了");
    assert_eq!(t.nrows(), 2);
    assert_eq!(t.row(1).unwrap()[1], Value::Text("b".into()));
}

#[test]
fn a_csv_table_survives_a_heap_mode_session() {
    // 格式跟着**表**走:堆模式的会话照旧读得到 CSV 表,而且**不会**把它搬走
    let dir = TempDir::new("csv-kept");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&csv_service(&service), "d", &bob);
        session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (7);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    {
        // 默认(堆)模式的会话:读得到,而且没把它迁走
        let mut session = open_session(&service, "d", &bob);
        let t = session.symbols().relation("t").expect("t 回来了");
        assert_eq!(t.row(0).unwrap()[0], Value::Int(7));
        // 在这个会话里再建一张表:它是堆表(两种格式同库并存)
        session.run_script("CREATE TABLE u (y INT); INSERT INTO u VALUES (9);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    assert!(lib.join("t.csv").is_file(), "CSV 表还在");
    assert!(!lib.join("t.heap").exists(), "CSV 表不该被迁成堆表");
    assert!(lib.join("u.heap").is_file(), "堆模式新建的是堆表");

    // 两种格式各读一遍(库上只有一个写者,两次会话要**先后**开)
    {
        let session = open_session(&service, "d", &bob);
        assert_eq!(session.symbols().relation("t").unwrap().row(0).unwrap()[0], Value::Int(7));
        assert_eq!(session.symbols().relation("u").unwrap().row(0).unwrap()[0], Value::Int(9));
    }
    // CSV 模式的会话反过来读那张堆表,也一样读得到
    let csv_only = open_session(&csv_service(&service), "d", &bob);
    assert_eq!(
        csv_only.symbols().relation("t").unwrap().row(0).unwrap()[0],
        Value::Int(7)
    );
    assert_eq!(
        csv_only.symbols().relation("u").unwrap().row(0).unwrap()[0],
        Value::Int(9)
    );
}

#[test]
fn a_legacy_csv_database_is_adopted_in_place_in_csv_mode() {
    // 老库(只有 `<表>.csv`,提交记录里没有格式记录)+ CSV 模式 = **一个字节都不搬**
    let dir = TempDir::new("csv-adopt");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");
    write_legacy_csv(&lib);

    let session = open_session(&csv_service(&service), "d", &bob);
    let t = session.symbols().relation("t").expect("t 回来了");
    assert_eq!(t.nrows(), 2);
    assert!(lib.join("t.csv").is_file(), "还是 CSV");
    assert!(!lib.join("t.heap").exists(), "没有搬成堆文件");
}

#[test]
fn a_legacy_csv_database_is_still_migrated_in_heap_mode() {
    // 同一个老库交给默认(堆)模式:照旧迁进堆文件,CSV 删掉
    let dir = TempDir::new("csv-migrate");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");
    write_legacy_csv(&lib);

    let session = open_session(&service, "d", &bob);
    let t = session.symbols().relation("t").expect("t 回来了");
    assert_eq!(t.nrows(), 2);
    assert!(lib.join("t.heap").is_file(), "搬进堆文件了");
    assert!(!lib.join("t.csv").exists(), "老 CSV 该删掉");
}

#[test]
fn a_csv_table_whose_file_row_count_changed_is_refused() {
    // 提交记录记着这张表有多少行:对不上就是有人手工动过目录,宁可开不了库
    let dir = TempDir::new("csv-rows");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");
    write_legacy_csv(&lib);
    std::fs::write(
        lib.join("manifest.json"),
        r#"{"version":2,"tables":{"t":{"csv":true,"csv_rows":2}},"promoted":[],"removed":[]}"#,
    )
    .unwrap();

    // 手工多加一行:文件说 3,记录说 2(对账在**读回来**那一步)
    std::fs::write(lib.join("t.csv"), "x,note\n1,a\n2,b\n3,c\n").unwrap();
    let db = csv_service(&service).open("d", &bob).expect("目录还在");
    let err = match db.load_tables() {
        Ok(tables) => panic!("行数对不上就该拒绝(读回来 {} 张表)", tables.len()),
        Err(e) => e,
    };
    assert!(err.to_string().contains("2") && err.to_string().contains("3"), "{err}");
}

#[test]
fn a_legacy_row_log_refuses_to_be_migrated() {
    // 7.7 那一版的 `<表>.log` 是行日志(数据在里面):本版重放不了,只能拒绝迁移
    // --迁了就是静默把那几行丢掉
    let dir = TempDir::new("legacy-log");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");
    write_legacy_csv(&lib);
    std::fs::write(lib.join("t.log"), "U,0,1,a\n").unwrap();

    let err = match service.open("d", &bob) {
        Ok(_) => panic!("老日志非空就该拒绝迁移"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("t.log"), "{err}");
    assert!(lib.join("t.csv").is_file(), "拒绝之后老数据一个字节都不动");
    assert!(!lib.join("t.heap").exists());
}

/// 手搓一个 TODO 7.5 之前的老库:一份 `<表>.csv` + 模式文件 + 没有格式记录的清单.
fn write_legacy_csv(lib: &Path) {
    std::fs::create_dir_all(lib).unwrap();
    std::fs::write(lib.join("t.csv"), "x,note\n1,a\n2,b\n").unwrap();
    std::fs::write(
        lib.join("t.schema.json"),
        r#"{"version":1,"name":"t","columns":[{"name":"x","ty":"INT"},{"name":"note","ty":"TEXT"}]}"#,
    )
    .unwrap();
    std::fs::write(
        lib.join("manifest.json"),
        r#"{"version":2,"tables":{"t":{"base":2,"log":0}},"promoted":[],"removed":[]}"#,
    )
    .unwrap();
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
    assert!(lib.join("movies.heap").is_file(), "页式堆文件该在(TODO 7.5)");
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
fn a_heap_with_too_many_dead_bytes_is_compacted() {
    // 堆文件是**只追加**的:`UPDATE` 写一条新记录,旧的那条留在文件里变成"死字节"
    // (TODO 7.5).死字节比活着的还多(而且文件本身够大)时,提交点顺手把整张表按
    // 当前行目录**密集重写**一遍--不整理的话每一次读都要跨过那些旧版本.
    let dir = TempDir::new("compact");
    let service = dir.service();
    service.create_database("d", "bob").unwrap();
    let bob = user("bob", true);
    let lib = dir.path().join("d");

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("CREATE TABLE t (x INT, pad TEXT);");
        // 一次事务灌 2000 行 x 2 KB:堆文件约 4 MB
        let mut script = String::from("BEGIN;\n");
        for i in 0..2000 {
            script.push_str(&format!("INSERT INTO t VALUES ({}, '{}');\n", i, "p".repeat(2000)));
        }
        script.push_str("COMMIT;");
        session.run_script(&script);
        assert_eq!(session.failed(), 0, "{}", session.messages());

        // 改两遍:每一遍都给 2000 行写上一条新记录(旧的那条变成死字节)
        session.run_script("BEGIN; UPDATE t SET pad = pad || 'a'; UPDATE t SET pad = pad || 'b'; COMMIT;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    let manifest = Manifest::load(&lib).unwrap().expect("提交记录在");
    let footprint = manifest.tables.get("t").copied().expect("t 的位置");
    assert_eq!(footprint.heap_rows, 2000, "行还在");
    let size = std::fs::metadata(lib.join("t.heap")).unwrap().len();
    assert!(
        size < footprint.heap_bytes.max(4 * 1024 * 1024) + 8 * 1024 * 1024,
        "整理之后文件该明显变小(现在 {size} 字节)"
    );

    // 另一个会话把 2000 行读回来,每一行的 `pad` 都是最后一次改的样子
    let session = open_session(&service, "d", &bob);
    let table = session.symbols().relation("t").unwrap();
    assert_eq!(table.nrows(), 2000);
    let pad = table.row(7).unwrap()[1].clone();
    assert_eq!(pad, Value::Text("p".repeat(2000) + "ab"), "{pad:?}");
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
        assert!(lib.join("study.movies.heap").is_file(), "建表就该落盘");
    }

    {
        let mut session = open_session(&service, "d", &bob);
        session.run_script("DROP SCHEMA study CASCADE;");
        assert!(session.messages().contains("schema study 已删除"), "{}", session.messages());
    }

    assert!(!lib.join("study.movies.heap").exists(), "表没了,文件也要没");
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
    assert!(!lib.join("s.t.heap").exists(), "压根没落盘过");

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

    // 会话还开着(锁也还握着):盘上的堆文件**已经**长出来了
    //
    // 行不再是 CSV 里的一行,而是堆文件里的一条记录(只追加,TODO 7.5);`9` 落在
    // `t.heap` 里,同时每个会话都能把它读回来.
    let heap = std::fs::read(dir.path().join("d/t.heap")).expect("t.heap 应当已经在盘上");
    assert!(!heap.is_empty(), "堆文件里该有那条记录");
    let footprint = Manifest::load(&dir.path().join("d"))
        .unwrap()
        .unwrap()
        .tables
        .get("t")
        .copied()
        .unwrap_or_default();
    assert_eq!(footprint.heap_rows, 1, "提交记录里也要算上这一行");

    drop(session);
    let session2 = open_session(&service, "d", &bob);
    assert_eq!(
        session2.symbols().relation("t").unwrap().row(0).unwrap()[0],
        Value::Int(9),
        "另一个会话也要读得到"
    );

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
    assert!(lib.join("s.t.heap").is_file(), "s.t 没落盘");
    assert!(lib.join("t.heap").is_file(), "t 没落盘");

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

    // 手工把 a 的两个文件复制成 b:b 的模式文件里写的还是表名 a
    let lib = dir.path().join("d");
    std::fs::copy(lib.join("a.heap"), lib.join("b.heap")).unwrap();
    std::fs::copy(lib.join("a.schema.json"), lib.join("b.schema.json")).unwrap();

    let db = service.open("d", &bob).expect("目录还在");
    let err = match db.load_tables() {
        Ok(tables) => panic!("主干名与表名对不上就该报错(读回来 {} 张表)", tables.len()),
        Err(e) => e,
    };
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

        // 事务外:建表当场落盘(建表就是提交,堆文件与模式文件都写出来)
        session.run_script("CREATE TABLE t (x INT);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        assert!(lib.join("t.heap").is_file());
        let empty = std::fs::metadata(lib.join("t.heap")).unwrap().len();

        // 事务里:插行会写进堆文件(追加式写落笔就写),但**提交记录还没算它**--
        // 所以崩在这一刻,重启之后那一行不算数(见 `pers::heap` 的文件头)
        session.run_script("BEGIN; INSERT INTO t VALUES (1);");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        let rows = |lib: &Path| {
            Manifest::load(lib)
                .unwrap()
                .and_then(|manifest| manifest.tables.get("t").copied())
                .map_or(0, |footprint| footprint.heap_rows)
        };
        assert_eq!(rows(&lib), 0, "事务里那几行还没提交");

        // `COMMIT` 一次把提交记录往前挪
        session.run_script("COMMIT;");
        assert_eq!(session.failed(), 0, "{}", session.messages());
        assert_eq!(rows(&lib), 1, "提交之后才算数");
        assert!(std::fs::metadata(lib.join("t.heap")).unwrap().len() > empty);
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

/// 给 `<库>/<表>.heap` **追加**一条记录(手工造"崩在半路"的现场用).
///
/// 与 `HeapTab` 写下去的那一条一模一样:`[len u32][rid u32][行编码]`.
fn append_heap_record(lib: &Path, table: &str, rid: u32, row: &[Value], columns: &[Column]) {
    let payload = encode_row(&row.to_vec(), columns).expect("编码");
    let path = lib.join(format!("{table}.heap"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("打开堆文件");
    use std::io::Write as _;
    file.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    file.write_all(&rid.to_le_bytes()).unwrap();
    file.write_all(&payload).unwrap();
}

/// 造一次"提交崩在提交点**之前**"的现场:`t` 的改动已经写进堆文件,而提交记录
/// (每张表写到了哪儿)还没更新.
fn crash_before_the_manifest(lib: &Path) {
    let columns = vec![Column::new("x", DataType::Int)];
    // 第 0 行改成 2:追加一条**同 rid** 的新记录(它落在已提交长度之外)
    append_heap_record(lib, "t", 0, &[Value::Int(2)], &columns);
}

/// 造一次"提交崩在提交点**之后**"的现场:提交记录里的长度已经跟上了那一条新记录,
/// 但"要推广的暂存文件"那一半还没做.
fn crash_after_the_manifest(lib: &Path) {
    let before = Manifest::load(lib).unwrap().expect("提交记录在");
    let footprint = before.tables.get("t").copied().unwrap_or_default();

    crash_before_the_manifest(lib);
    let size = std::fs::metadata(lib.join("t.heap")).unwrap().len();

    let mut manifest = before;
    manifest.tables.insert(
        "t".to_string(),
        TableFootprint {
            heap: true,
            heap_bytes: size,
            heap_rows: footprint.heap_rows.max(1),
            moved: footprint.moved,
            ..Default::default()
        },
    );
    manifest.save(lib).expect("写提交记录");
}

/// 造一次"提交记录说该推广,而推广还没做"的现场:`<表>.heap.new` 里是一份**密集的**
/// 新堆文件(第 0 行是 2),清单里记着要推广它.
fn crash_with_a_staged_heap(lib: &Path) {
    let shape = ColumnsShape::new(vec![Column::new("x", DataType::Int)]);
    let mut pending = PhysTab::new("t", &shape);
    Table::insert(&mut pending, vec![Value::Int(2)]).unwrap();
    let bytes = ydbms::pers::heap::HeapTab::write_dense(&lib.join("t.heap"), &pending).unwrap();

    let mut manifest = Manifest::load_or_empty(lib).unwrap();
    manifest.tables.insert(
        "t".to_string(),
        TableFootprint {
            heap: true,
            heap_bytes: bytes,
            heap_rows: 1,
            moved: 0,
            ..Default::default()
        },
    );
    manifest.promoted = vec!["t".to_string()];
    manifest.save(lib).expect("写提交记录");
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
    let (dir, service, bob) = crash_fixture("crash-heap");
    let lib = dir.path().join("d");

    // 改动已经写进堆文件,提交记录还没更新:那份改动**不算数**
    let committed = std::fs::metadata(lib.join("t.heap")).unwrap().len();
    crash_before_the_manifest(&lib);
    assert!(
        std::fs::metadata(lib.join("t.heap")).unwrap().len() > committed,
        "现场:堆文件长出来一截"
    );

    // 打开库:账本之外的那一截被截掉,`t` 还是提交前那一份
    let session = open_session(&service, "d", &bob);
    assert_eq!(session.symbols().relation("t").unwrap().nrows(), 1);
    assert_eq!(
        session.symbols().relation("t").unwrap().row(0).unwrap()[0],
        Value::Int(1),
        "崩在提交点之前 = 旧数据"
    );
    assert_eq!(
        std::fs::metadata(lib.join("t.heap")).unwrap().len(),
        committed,
        "没越过提交点的那条记录要截掉"
    );
}

#[test]
fn a_crash_after_the_manifest_keeps_the_appended_rows() {
    let (dir, service, bob) = crash_fixture("crash-manifest");
    let lib = dir.path().join("d");

    // 改动写进堆文件 + 提交记录里的长度也跟上了:这份改动**已经提交**
    crash_after_the_manifest(&lib);

    let session = open_session(&service, "d", &bob);
    let t = session.symbols().relation("t").unwrap();
    assert_eq!(t.nrows(), 1);
    assert_eq!(t.row(0).unwrap()[0], Value::Int(2), "崩在提交点之后 = 新数据");
    assert_eq!(
        session.symbols().relation("u").unwrap().row(0).unwrap()[0],
        Value::Int(7),
        "没被这次提交碰过的表照旧"
    );
    // 现场本来就已经落定(这次提交没有要推广的东西)
    let manifest = Manifest::load(&lib).unwrap().expect("提交记录在");
    assert!(manifest.settled(), "记录里不该留着没做完的记号");
}

#[test]
fn a_crash_in_the_middle_of_promoting_still_ends_after_the_commit() {
    let (dir, service, bob) = crash_fixture("crash-promote");
    let lib = dir.path().join("d");

    // 提交记录写了(要推广 `t`),推广还没做
    crash_with_a_staged_heap(&lib);
    assert!(lib.join("t.heap.new").is_file(), "现场:暂存的堆文件在");

    let session = open_session(&service, "d", &bob);
    assert_eq!(
        session.symbols().relation("t").unwrap().row(0).unwrap()[0],
        Value::Int(2),
        "补完推广之后读到的是暂存的那一份"
    );
    assert!(!lib.join("t.heap.new").exists(), "补完要把剩下的搬完");
    assert!(Manifest::load(&lib).unwrap().unwrap().settled(), "现场要落定");
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
