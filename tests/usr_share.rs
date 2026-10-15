//! 多用户共享的 testbench(6.4):走**会话**的授权与权限判定.
//!
//! 覆盖:表级 / 列级的 `GRANT` / `REVOKE`,谁有资格授,`CONNECT` 与"连不上",DDL 与
//! 表权限的分工,owner 与超级用户绕过,`DROP OWNED BY` + `DROP ROLE` 的配套,
//! 跨库的 `GRANT CONNECT` 与 `ALTER DATABASE ... OWNER TO`,表没了 / 同名新表的
//! 授权清理,事务里拒绝,以及内存模式的拒绝.
//!
//! 授权表本身的细节在 `tests/usr_grants.rs`;列使用分析在 `tests/ir_columns.rs`;
//! 跨进程(真 `ysql`)在 `tests/ysql_cli.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::cmd::{Session, Sink};
use ydbms::usr::service::ServiceDir;
use ydbms::usr::{RoleAttrs, User, UserStore};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录(每个测试一个**服务目录**).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-share-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn service(&self) -> ServiceDir {
        ServiceDir::resolve(Some(self.path.to_path_buf()), None).expect("服务目录")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 普通用户:能登录.
fn plain() -> RoleAttrs {
    RoleAttrs {
        admin: false,
        login: true,
        create_db: false,
        create_role: false,
        inherit: true,
    }
}

/// 建一个角色再把记录读回来(建会话要用用户表里那一份).
fn add(service: &ServiceDir, name: &str, attrs: RoleAttrs) -> User {
    if !service.users_path().is_file() {
        UserStore::empty().save(&service.users_path()).expect("写空用户表");
    }
    service
        .create_role(name, Some("pw"), attrs, &[])
        .expect("建角色");
    service
        .users()
        .expect("读用户表")
        .find(name)
        .expect("刚建的")
        .clone()
}

/// 打开一个库并起一个会话.
fn open(service: &ServiceDir, database: &str, user: &User) -> Session {
    let db = service.open(database, user).expect("打开库");
    Session::open_with_sinks(
        db,
        user.clone(),
        service.clone(),
        Sink::buffer(),
        Sink::buffer(),
    )
    .expect("起会话")
}

/// 一个"标准现场":超级用户 `bob`,库 `filmdb`(owner 是普通用户 `owner`),
/// 库里两张表,`alice` 是个还没被授权的普通用户.
struct Scene {
    dir: TempDir,
    service: ServiceDir,
    bob: User,
    owner: User,
    alice: User,
}

fn scene(tag: &str) -> Scene {
    let dir = TempDir::new(tag);
    let service = dir.service();
    let bob = add(&service, "bob", RoleAttrs::bootstrap());
    let owner = add(&service, "owner", plain());
    let alice = add(&service, "alice", plain());
    service.create_database("filmdb", "owner").expect("建库");

    {
        let mut session = open(&service, "filmdb", &owner);
        session.run_script(
            "CREATE TABLE movies (movie_id INT, title TEXT, year INT);\n\
             INSERT INTO movies VALUES (1, 'Alien', 1979), (2, 'Aliens', 1986);",
        );
        assert_eq!(session.failed(), 0, "{}", session.messages());
    }

    Scene {
        dir,
        service,
        bob,
        owner,
        alice,
    }
}

// ------------------------------------------------------------ 表级授权

#[test]
fn a_granted_user_reads_but_cannot_write() {
    let s = scene("select-only");

    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "GRANT CONNECT ON DATABASE filmdb TO alice;\n\
             GRANT SELECT ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
        assert!(owner.messages().contains("可以连库"), "{}", owner.messages());
    }

    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT movie_id, title FROM movies ORDER BY movie_id;");
    assert_eq!(alice.failed(), 0, "{}", alice.messages());
    assert!(alice.query_output().contains("Alien"), "{}", alice.query_output());

    alice.run_script("INSERT INTO movies VALUES (3, 'Alien3', 1992);");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    assert!(alice.messages().contains("INSERT"), "{}", alice.messages());
    assert!(alice.messages().contains("alice"), "{}", alice.messages());

    alice.run_script("UPDATE movies SET title = 'x';");
    assert!(alice.messages().contains("UPDATE"), "{}", alice.messages());

    // 行数没变(拒绝发生在动数据之前)
    drop(alice);
    let check = open(&s.service, "filmdb", &s.owner);
    assert_eq!(check.symbols().relation("movies").unwrap().nrows(), 2);
}

#[test]
fn revoke_takes_the_privilege_back() {
    let s = scene("revoke");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "GRANT CONNECT ON DATABASE filmdb TO alice;\n\
             GRANT SELECT ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }

    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT movie_id FROM movies;");
    assert_eq!(alice.failed(), 0, "{}", alice.messages());
    drop(alice);

    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("REVOKE SELECT ON movies FROM alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
        assert!(owner.messages().contains("已收回"), "{}", owner.messages());
    }

    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT movie_id FROM movies;");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
}

#[test]
fn a_user_without_connect_cannot_open_the_database() {
    let s = scene("no-connect");
    let err = s
        .service
        .open("filmdb", &s.alice)
        .expect_err("没授 CONNECT 就连不上");
    assert!(err.to_string().contains("alice"), "{err}");

    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("GRANT CONNECT ON DATABASE filmdb TO alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
        // 表的权限一点没给:连得上,但读不了
    }

    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT movie_id FROM movies;");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    assert!(alice.messages().contains("SELECT"), "{}", alice.messages());
}

#[test]
fn table_grants_do_not_grant_ddl() {
    let s = scene("no-ddl");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "GRANT CONNECT ON DATABASE filmdb TO alice;\n\
             GRANT SELECT, INSERT, UPDATE ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }

    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("CREATE TABLE own (x INT);");
    assert!(alice.messages().contains("CREATE TABLE"), "{}", alice.messages());
    alice.run_script("CREATE INDEX i ON movies (title);");
    assert!(alice.messages().contains("CREATE INDEX"), "{}", alice.messages());
    assert!(
        !s.dir.path().join("filmdb/own.csv").exists(),
        "拒绝的语句不该建出文件"
    );

    // 表权限照旧
    alice.run_script("INSERT INTO movies VALUES (3, 'Alien3', 1992);");
    assert_eq!(alice.failed(), 2, "{}", alice.messages());
}

#[test]
fn the_owner_and_the_superuser_bypass_grants() {
    let s = scene("bypass");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("INSERT INTO movies VALUES (3, 'Alien3', 1992);");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }
    drop(s.bob.clone());
    let mut bob = open(&s.service, "filmdb", &s.bob);
    bob.run_script(
        "SELECT movie_id FROM movies;\n\
         INSERT INTO movies VALUES (4, 'Alien4', 1997);\n\
         CREATE TABLE extra (x INT);",
    );
    assert_eq!(bob.failed(), 0, "{}", bob.messages());
}

// ------------------------------------------------------------ 列级授权

#[test]
fn column_level_grants_are_per_column() {
    let s = scene("column-level");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "GRANT CONNECT ON DATABASE filmdb TO alice;\n\
             GRANT SELECT (title) ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
        assert!(
            owner.messages().contains("SELECT(title)"),
            "状态行要说清授了哪几列:{}",
            owner.messages()
        );
    }

    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT title FROM movies ORDER BY title;");
    assert_eq!(alice.failed(), 0, "{}", alice.messages());
    assert!(alice.query_output().contains("Alien"), "{}", alice.query_output());

    alice.run_script("SELECT movie_id FROM movies;");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    assert!(alice.messages().contains("movie_id"), "{}", alice.messages());

    alice.run_script("SELECT * FROM movies;");
    assert_eq!(alice.failed(), 2, "{}", alice.messages());

    // `WHERE` 里读到的列也要权限
    alice.run_script("SELECT title FROM movies WHERE year > 1980;");
    assert_eq!(alice.failed(), 3, "{}", alice.messages());
    assert!(alice.messages().contains("year"), "{}", alice.messages());
    drop(alice);

    // 列级的 `INSERT`:写了别的列就不行
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("GRANT INSERT (title) ON movies TO alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }
    let mut alice = open(&s.service, "filmdb", &s.alice);
    // 授了列级的 `INSERT (title)`:只写那一列可以,写整行不行
    alice.run_script("INSERT INTO movies (title) VALUES ('Prometheus');");
    assert_eq!(alice.failed(), 0, "{}", alice.messages());
    alice.run_script("INSERT INTO movies VALUES (5, 'Covenant', 2017);");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    assert!(alice.messages().contains("movie_id") || alice.messages().contains("year"), "{}", alice.messages());

    // `UPDATE`:写到的列要 `UPDATE`,读到的列要 `SELECT`(-- 两个都没授)
    alice.run_script("UPDATE movies SET title = 'x' WHERE title = 'Alien';");
    assert_eq!(alice.failed(), 2, "{}", alice.messages());
    assert!(alice.messages().contains("UPDATE"), "{}", alice.messages());
}

#[test]
fn a_table_level_grant_covers_every_column() {
    let s = scene("table-covers-columns");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "GRANT CONNECT ON DATABASE filmdb TO alice;\n\
             GRANT SELECT ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }
    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT * FROM movies WHERE year > 1980;");
    assert_eq!(alice.failed(), 0, "{}", alice.messages());
}

#[test]
fn a_query_that_reads_no_column_still_needs_a_privilege_on_the_table() {
    let s = scene("count-star");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("GRANT CONNECT ON DATABASE filmdb TO alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }

    // 一点表权限都没有:连得上也算不出来
    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT COUNT(*) FROM movies;");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    drop(alice);

    // 授了任意一列的 SELECT,`count(*)` 就过得去(它没读具体哪一列)
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("GRANT SELECT (title) ON movies TO alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }
    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("SELECT COUNT(*) FROM movies;");
    assert_eq!(alice.failed(), 0, "{}", alice.messages());
}

// -------------------------------------------------- 授权表的寿命与"谁有资格"

#[test]
fn a_dropped_table_loses_its_grants_and_a_new_one_starts_clean() {
    let s = scene("table-lifetime");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "CREATE SCHEMA study;\n\
             SET search_path TO study;\n\
             CREATE TABLE movies (movie_id INT, title TEXT);\n\
             GRANT SELECT ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }

    // 手工塞一条**同名但表已经没了**的残留:新建同名表时必须被清掉
    let path = s.dir.path().join("filmdb/grants.json");
    let mut grants = ydbms::usr::Grants::load(&s.dir.path().join("filmdb"))
        .expect("读授权表")
        .expect("文件在");
    grants.set_table(
        "gone",
        "alice",
        ydbms::usr::Privilege::Select,
        &[],
        true,
    );
    grants.save(&s.dir.path().join("filmdb")).expect("写回");
    assert!(path.is_file());

    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "CREATE TABLE gone (x INT);\n\
             DROP SCHEMA study CASCADE;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }

    let back = ydbms::usr::Grants::load(&s.dir.path().join("filmdb"))
        .expect("读授权表")
        .expect("文件在");
    assert!(
        !back.tables.iter().any(|grant| grant.table == "gone"),
        "同名新表不许继承旧授权:{:?}",
        back.tables
    );
    assert!(
        !back.tables.iter().any(|grant| grant.table == "study.movies"),
        "表没了,授权也要没:{:?}",
        back.tables
    );
}

#[test]
fn only_the_owner_or_a_superuser_may_grant() {
    let s = scene("who-may-grant");
    // alice 连得上(先由 owner 授 CONNECT),但她自己不能授
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("GRANT CONNECT ON DATABASE filmdb TO alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }
    let mut alice = open(&s.service, "filmdb", &s.alice);
    alice.run_script("GRANT SELECT ON movies TO bob;");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    assert!(alice.messages().contains("授权"), "{}", alice.messages());

    // 收别人的授权也要 `CREATEROLE`(超级用户或建角色的那一位)
    alice.run_script("DROP OWNED BY bob;");
    assert_eq!(alice.failed(), 2, "{}", alice.messages());
    assert!(alice.messages().contains("CREATEROLE"), "{}", alice.messages());
    drop(alice);

    let mut owner = open(&s.service, "filmdb", &s.owner);
    owner.run_script("GRANT SELECT ON movies TO nobody;");
    assert_eq!(owner.failed(), 1, "{}", owner.messages());
    assert!(owner.messages().contains("nobody"), "{}", owner.messages());
}

#[test]
fn drop_owned_by_clears_the_way_for_drop_role() {
    let s = scene("drop-owned");
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script(
            "GRANT CONNECT ON DATABASE filmdb TO alice;\n\
             GRANT SELECT ON movies TO alice;",
        );
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }

    // 还留着授权:删不掉,而且点名卡在哪个库
    {
        let mut bob = open(&s.service, "filmdb", &s.bob);
        bob.run_script("DROP ROLE alice;");
        assert_eq!(bob.failed(), 1, "{}", bob.messages());
        assert!(bob.messages().contains("filmdb"), "{}", bob.messages());

        // `DROP OWNED BY` 清干净(超级用户做),然后就能删了
        bob.run_script("DROP OWNED BY alice;");
        assert_eq!(bob.failed(), 1, "{}", bob.messages());
        assert!(bob.messages().contains("已收回"), "{}", bob.messages());

        bob.run_script("DROP ROLE alice;");
        assert_eq!(bob.failed(), 1, "{}", bob.messages());
    }
    assert!(s.service.users().unwrap().find("alice").is_none());

    let back = ydbms::usr::Grants::load(&s.dir.path().join("filmdb"))
        .unwrap()
        .unwrap();
    assert!(!back.mentions("alice"));
}

// ------------------------------------------------------------ 跨库

#[test]
fn connect_can_be_granted_from_another_database() {
    let s = scene("cross-db");
    // bob 连着自己的库,给别的库(filmdb,owner 是 owner)授权
    s.service.create_database("bobdb", "bob").expect("建库");
    let mut bob = open(&s.service, "bobdb", &s.bob);
    bob.run_script("GRANT CONNECT ON DATABASE filmdb TO alice;");
    assert_eq!(bob.failed(), 0, "{}", bob.messages());
    assert!(bob.messages().contains("filmdb"), "{}", bob.messages());

    assert!(s.service.open("filmdb", &s.alice).is_ok(), "现在连得上了");

    // 库正被别人连着:借不到锁,`GRANT` 明确拒绝(授权表在外人手里)
    {
        let held = s.service.open("filmdb", &s.owner).expect("owner 连上");
        bob.run_script("GRANT CONNECT ON DATABASE filmdb TO alice;");
        assert_eq!(bob.failed(), 1, "{}", bob.messages());
        assert!(bob.messages().contains("另一个进程"), "{}", bob.messages());
        drop(held);
    }

    // 别的库的 `CONNECT` 也能收
    bob.run_script("REVOKE CONNECT ON DATABASE filmdb FROM alice;");
    assert_eq!(bob.failed(), 1, "{}", bob.messages());
    assert!(s.service.open("filmdb", &s.alice).is_err());

    // 跨库改 owner 也行(超级用户)
    bob.run_script("ALTER DATABASE filmdb OWNER TO alice;");
    assert_eq!(bob.failed(), 1, "{}", bob.messages());
    assert_eq!(s.service.catalog("filmdb").unwrap().owner, "alice");
}

#[test]
fn alter_database_owner_needs_superuser_or_the_current_owner() {
    let s = scene("alter-owner");
    s.service.create_database("other", "owner").expect("建库");

    // alice 连着她自己的库(她既不是 `other` 的 owner,也不是超级用户)
    s.service.create_database("alicedb", "alice").expect("建库");
    let mut alice = open(&s.service, "alicedb", &s.alice);
    alice.run_script("ALTER DATABASE other OWNER TO alice;");
    assert_eq!(alice.failed(), 1, "{}", alice.messages());
    assert_eq!(s.service.catalog("other").unwrap().owner, "owner");

    // owner 自己可以(换成 alice),换完 alice 就是 owner 了
    drop(alice);
    {
        let mut owner = open(&s.service, "filmdb", &s.owner);
        owner.run_script("ALTER DATABASE other OWNER TO alice;");
        assert_eq!(owner.failed(), 0, "{}", owner.messages());
    }
    assert_eq!(s.service.catalog("other").unwrap().owner, "alice");
    assert!(s.service.open("other", &s.alice).is_ok(), "owner 连得上");

    // 不存在的角色:拒绝,owner 不变
    let mut bob = open(&s.service, "filmdb", &s.bob);
    bob.run_script("ALTER DATABASE other OWNER TO nobody;");
    assert!(bob.messages().contains("nobody"), "{}", bob.messages());
    assert_eq!(s.service.catalog("other").unwrap().owner, "alice");
}

// ------------------------------------------------------------ 事务与内存模式

#[test]
fn authorization_statements_are_refused_inside_a_transaction() {
    let s = scene("grants-tx");
    let mut owner = open(&s.service, "filmdb", &s.owner);
    for sql in [
        "BEGIN; GRANT SELECT ON movies TO alice;",
        "BEGIN; REVOKE SELECT ON movies FROM alice;",
        "BEGIN; DROP OWNED BY alice;",
        "BEGIN; ALTER DATABASE filmdb OWNER TO alice;",
        "BEGIN; GRANT CONNECT ON DATABASE filmdb TO alice;",
    ] {
        owner.run_script(sql);
        assert!(
            owner.messages().contains("不能在事务里跑"),
            "{sql}:{}",
            owner.messages()
        );
        owner.run_script("ROLLBACK;");
    }
}

#[test]
fn a_memory_session_refuses_grant_statements_but_still_runs_sql() {
    let mut session = Session::with_sinks(Sink::buffer(), Sink::buffer());
    session.run_script(
        "CREATE TABLE movies (movie_id INT);\n\
         INSERT INTO movies VALUES (1);",
    );
    assert_eq!(session.failed(), 0, "{}", session.messages());

    session.run_script(
        "GRANT SELECT ON movies TO alice;\n\
         DROP OWNED BY alice;\n\
         ALTER DATABASE filmdb OWNER TO alice;",
    );
    assert!(session.messages().contains("没有服务目录"), "{}", session.messages());
    assert_eq!(session.failed(), 3, "{}", session.messages());

    // 内存模式没有用户:表权限检查放行
    session.run_script("SELECT movie_id FROM movies;");
    assert_eq!(session.failed(), 3, "{}", session.messages());
    assert!(session.query_output().contains('1'), "{}", session.query_output());
}
