//! 角色系统的 testbench(阶段 6.4).
//!
//! 覆盖:`users.json` 的新字段与**老文件的向后兼容**,口令 / `NOLOGIN` 两条登录口径,
//! 成员关系(可达,`INHERIT` 逐边判断,防环),`CREATE / ALTER / DROP ROLE` 的**结构**
//! 检查(最后一个超级用户,有库 / 有授权的角色不能改名 / 删),`users.json` 的写锁,
//! 以及 `SET ROLE` / `RESET ROLE` 的会话效果(含事务回滚盖回角色).
//!
//! 权限判定(谁能建角色,谁能动结构)在 `tests/usr_share.rs`;跨进程在 `tests/ysql_cli.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::cmd::{Session, Sink};
use ydbms::usr::service::ServiceDir;
use ydbms::usr::{
    roles, AuthError, Password, PasswordAction, RoleAttrChange, RoleAttrs, User, UserStore,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录(每个测试一个**服务目录**).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-roles-{}-{}-{}", std::process::id(), tag, n));
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

    /// 手搓一份 `users.json`(测老文件,也测锁).
    fn write_users(&self, text: &str) {
        std::fs::write(self.path.join("users.json"), text).expect("写 users.json");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 一套属性:超级用户 / 普通用户,外加能不能建库建角色.
fn attrs(admin: bool, login: bool) -> RoleAttrs {
    RoleAttrs {
        admin,
        login,
        create_db: admin,
        create_role: admin,
        inherit: true,
    }
}

/// 建一个角色,再把它的记录读回来(**建会话要用用户表里那一份**,不是手搓的).
///
/// `ysql` 的首次启动会先把 `users.json` 建出来(bootstrap),这里照做:角色那几条
/// 语句**不**替人凭空造一份用户表(见 `role_statements_need_a_user_table`).
fn add(service: &ServiceDir, name: &str, attrs: RoleAttrs) -> User {
    bootstrap(service);
    service
        .create_role(name, Some("pw"), attrs, &[])
        .expect("建角色");
    service.users().expect("读用户表").find(name).expect("刚建的").clone()
}

/// 让服务目录里有一份空的 `users.json`(等价于"已经 bootstrap 过").
fn bootstrap(service: &ServiceDir) {
    if !service.users_path().is_file() {
        UserStore::empty().save(&service.users_path()).expect("写空用户表");
    }
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

// ------------------------------------------------------------ 属性与兼容

#[test]
fn an_old_users_json_still_reads_back_with_the_new_defaults() {
    // 后加的那五个字段必须让老文件**行为不变**:能登录,能继承,没有成员关系
    let dir = TempDir::new("old-users");
    dir.write_users(
        r#"{"version": 1, "users": [{"name": "bob", "salt": "ab", "password_hash": "cd",
             "is_admin": true, "created_unix": 7}]}"#,
    );

    let store = UserStore::load(&dir.path().join("users.json")).expect("读回");
    let bob = store.find("bob").expect("bob 在");
    assert!(bob.can_login, "老文件里的人要能登录");
    assert!(bob.inherit, "老文件里的人默认继承");
    assert!(!bob.can_create_db && !bob.can_create_role, "新开关默认关");
    assert!(bob.member_of.is_empty());
    assert_eq!(bob.created_unix, 7);
    assert_eq!(bob.attrs().admin, true);
}

#[test]
fn the_three_role_kinds_have_their_own_defaults() {
    assert!(RoleAttrs::user().login, "CREATE USER 默认能登录");
    assert!(!RoleAttrs::role().login, "CREATE ROLE 默认不能登录");
    assert!(!RoleAttrs::user().admin);

    let boot = RoleAttrs::bootstrap();
    assert!(boot.admin && boot.login && boot.create_db && boot.create_role && boot.inherit);
}

// ------------------------------------------------------------------ 登录

#[test]
fn a_role_without_a_password_and_a_nologin_role_cannot_log_in() {
    let mut store = UserStore::empty();
    store.add("nopw", None, RoleAttrs::user()).expect("建角色");
    assert!(matches!(
        store.authenticate("nopw", "whatever"),
        Err(AuthError::NoPassword { .. })
    ));

    store.add("role", Some("pw"), RoleAttrs::role()).expect("建角色");
    assert!(matches!(
        store.authenticate("role", "pw"),
        Err(AuthError::CannotLogin { .. })
    ));

    // 设了口令就能登录;清掉又不行(`PASSWORD NULL`)
    store.set_password("nopw", Some("s3cret")).expect("换口令");
    assert!(store.authenticate("nopw", "s3cret").is_ok());
    store.set_password("nopw", None).expect("清口令");
    assert!(matches!(
        store.authenticate("nopw", "s3cret"),
        Err(AuthError::NoPassword { .. })
    ));
}

// -------------------------------------------------------------- 成员关系

#[test]
fn membership_is_transitive_and_inherit_is_checked_per_edge() {
    let mut store = UserStore::empty();
    store.add("staff", None, RoleAttrs::role()).unwrap();
    store.add("readers", None, RoleAttrs::role()).unwrap();
    store.add("alice", Some("pw"), RoleAttrs::user()).unwrap();

    // alice ∈ readers,readers ∈ staff
    store.add_membership("readers", "alice").unwrap();
    store.add_membership("staff", "readers").unwrap();

    let held: Vec<String> = roles::members_of(&store, "alice").into_iter().collect();
    assert_eq!(held, vec!["readers".to_string(), "staff".to_string()]);
    let effective: Vec<String> = roles::effective_roles(&store, "alice").into_iter().collect();
    assert_eq!(
        effective,
        vec!["alice".to_string(), "readers".to_string(), "staff".to_string()]
    );
    assert!(roles::can_set_role(&store, "alice", "staff"));
    assert!(!roles::can_set_role(&store, "alice", "nope-of-course"));

    // NOINHERIT:成员关系还在(能 SET ROLE),但不自动带权限
    let mut noinherit = attrs(false, true);
    noinherit.inherit = false;
    store.add("bob", Some("pw"), noinherit).unwrap();
    store.add_membership("readers", "bob").unwrap();
    assert!(roles::members_of(&store, "bob").contains("readers"));
    assert_eq!(roles::effective_roles(&store, "bob").len(), 1, "只有他自己");
}

#[test]
fn a_membership_cycle_is_refused() {
    let mut store = UserStore::empty();
    store.add("a", None, RoleAttrs::role()).unwrap();
    store.add("b", None, RoleAttrs::role()).unwrap();
    store.add("c", None, RoleAttrs::role()).unwrap();

    store.add_membership("a", "b").unwrap(); // b ∈ a
    store.add_membership("b", "c").unwrap(); // c ∈ b
    assert!(store.add_membership("c", "a").is_err(), "c 已经能到 a,再加 a → c 就绕回来了");
    assert!(store.add_membership("a", "a").is_err(), "自己属于自己");

    // 拒绝的那些一个都没写进去
    assert!(!store.find("a").unwrap().member_of.contains(&"c".to_string()));
    assert!(!store.find("a").unwrap().member_of.contains(&"a".to_string()));
}

// ------------------------------------------------------ 建 / 改 / 删(结构)

#[test]
fn create_alter_drop_round_trips_through_users_json() {
    let dir = TempDir::new("round-trip");
    let service = dir.service();
    bootstrap(&service);

    service
        .create_role("readers", None, RoleAttrs::role(), &[])
        .expect("先建 readers");
    let message = service
        .create_role("alice", Some("hunter2"), attrs(false, true), &["readers".to_string()])
        .expect("建角色");
    assert!(message.contains("alice") && message.contains("能登录"), "{message}");
    assert_eq!(
        service.users().unwrap().find("alice").unwrap().member_of,
        ["readers"]
    );

    // `IN ROLE` 里那个角色必须先在(这里没有:报错)
    let err = service
        .create_role("carol", Some("pw"), attrs(false, true), &["nobody".to_string()])
        .expect_err("成员关系里的角色必须存在");
    assert!(err.to_string().contains("nobody"), "{err}");

    let raw = std::fs::read_to_string(dir.path().join("users.json")).expect("读文件");
    assert!(!raw.contains("hunter2"), "口令不能明文落盘:{raw}");

    // 改属性 + 换口令 + 改名
    let message = service
        .alter_role(
            "alice",
            Some("alice2"),
            PasswordAction::Set(Password::new("new-pw")),
            &[RoleAttrChange::CreateDb(true)],
        )
        .expect("改角色");
    assert!(message.contains("alice2"), "{message}");

    let store = service.users().expect("读用户表");
    assert!(store.find("alice").is_none(), "改过名了");
    let alice = store.find("alice2").expect("新名字在");
    assert!(alice.can_create_db, "CREATEDB 该生效");
    assert!(store.authenticate("alice2", "new-pw").is_ok(), "新口令");
    assert!(store.authenticate("alice2", "hunter2").is_err(), "旧口令作废");

    // 授 / 收成员关系(carol 还没建:上面那次 `IN ROLE nobody` 被拒了)
    service
        .create_role("carol", Some("pw"), attrs(false, true), &[])
        .expect("建 carol");
    service
        .grant_roles(&["alice2".to_string()], &["carol".to_string()], true)
        .expect("授成员");
    assert!(service.users().unwrap().find("carol").unwrap().member_of == ["alice2"]);
    service
        .grant_roles(&["alice2".to_string()], &["carol".to_string()], false)
        .expect("收成员");
    assert!(service.users().unwrap().find("carol").unwrap().member_of.is_empty());

    // 删(两次:第二次走 IF EXISTS)
    let message = service.drop_roles(&["carol".to_string()], false).expect("删角色");
    assert!(message.contains("carol"), "{message}");
    let message = service
        .drop_roles(&["carol".to_string()], true)
        .expect("IF EXISTS 不算错");
    assert!(message.contains("IF EXISTS") || message.contains("不存在"), "{message}");
    assert!(service.users().unwrap().find("carol").is_none());
}

#[test]
fn the_last_superuser_cannot_be_demoted_or_dropped() {
    let dir = TempDir::new("last-superuser");
    let service = dir.service();
    let root = add(&service, "root", RoleAttrs::bootstrap());
    let bob = add(&service, "bob", attrs(false, true));
    assert!(root.is_admin && !bob.is_admin);

    // root 是唯一的超级用户:降级 / 删都不行
    let err = service
        .alter_role("root", None, PasswordAction::Unchanged, &[RoleAttrChange::Admin(false)])
        .expect_err("最后一个超级用户不能降级");
    assert!(err.to_string().contains("最后一个超级用户"), "{err}");
    let err = service
        .drop_roles(&["root".to_string()], false)
        .expect_err("最后一个超级用户不能删");
    assert!(err.to_string().contains("最后一个超级用户"), "{err}");

    // 先把 bob 提成超级用户,root 就可以降级了
    service
        .alter_role("bob", None, PasswordAction::Unchanged, &[RoleAttrChange::Admin(true)])
        .expect("提 bob");
    service
        .alter_role("root", None, PasswordAction::Unchanged, &[RoleAttrChange::Admin(false)])
        .expect("现在可以降 root");
    assert!(!service.users().unwrap().find("root").unwrap().is_admin);
}

#[test]
fn a_role_that_owns_a_database_cannot_be_renamed_or_dropped() {
    let dir = TempDir::new("dirty-role");
    let service = dir.service();
    add(&service, "owner", attrs(false, true));
    service.create_database("filmdb", "owner").unwrap();

    let err = service
        .alter_role("owner", Some("boss"), PasswordAction::Unchanged, &[])
        .expect_err("有库的人不能改名");
    assert!(err.to_string().contains("filmdb"), "要点名卡在哪个库:{err}");
    let err = service
        .drop_roles(&["owner".to_string()], false)
        .expect_err("有库的人不能删");
    assert!(err.to_string().contains("filmdb"), "{err}");

    // 换个名字之后仍然不行(库的 owner 还是旧名字),但改口令可以
    assert!(service
        .alter_role("owner", None, PasswordAction::Set(Password::new("pw2")), &[])
        .is_ok());
    assert_eq!(service.catalog("filmdb").unwrap().owner, "owner");
}

#[test]
fn renaming_moves_the_references_in_other_roles() {
    let dir = TempDir::new("rename-refs");
    let service = dir.service();
    add(&service, "reader", attrs(false, true));
    add(&service, "carol", attrs(false, true));
    service
        .grant_roles(&["reader".to_string()], &["carol".to_string()], true)
        .unwrap();

    service
        .alter_role("reader", Some("bookworm"), PasswordAction::Unchanged, &[])
        .expect("改名");
    let store = service.users().unwrap();
    assert!(store.find("reader").is_none());
    assert_eq!(store.find("carol").unwrap().member_of, ["bookworm"]);
}

#[test]
fn role_statements_need_a_user_table() {
    // 用户表不在时**明确拒绝**,不凭空造一份:那样下一次启动会以为早就 bootstrap 过,
    // 谁都进不来
    let dir = TempDir::new("no-users-json");
    let service = dir.service();

    let err = service
        .create_role("alice", Some("pw"), attrs(false, true), &[])
        .expect_err("没有用户表就该拒绝");
    assert!(err.to_string().contains("用户表"), "{err}");
    assert!(!dir.path().join("users.json").exists(), "不该凭空造一份");
}

#[test]
fn two_processes_cannot_write_the_user_table_at_once() {
    let dir = TempDir::new("users-lock");
    let service = dir.service();
    add(&service, "root", RoleAttrs::bootstrap());

    // 拿着锁(另一个进程的现场),谁来都写不了
    let lock = std::fs::File::create(dir.path().join(".users.lock")).expect("建锁文件");
    lock.try_lock().expect("上锁");

    let err = service
        .create_role("alice", Some("pw"), attrs(false, true), &[])
        .expect_err("锁在别人手里");
    assert!(err.to_string().contains("另一个进程"), "{err}");

    drop(lock);
    assert!(service
        .create_role("alice", Some("pw"), attrs(false, true), &[])
        .is_ok());
}

// ------------------------------------------------------------ 会话里的角色

#[test]
fn a_superuser_creates_roles_and_memberships_from_a_session() {
    let dir = TempDir::new("session-roles");
    let service = dir.service();
    let bob = add(&service, "bob", RoleAttrs::bootstrap());
    service.create_database("bob", "bob").unwrap();

    let mut session = open(&service, "bob", &bob);
    session.run_script(
        "CREATE ROLE reader;\n\
         CREATE USER carol PASSWORD 'hunter2';\n\
         GRANT reader TO carol;\n\
         ALTER ROLE carol INHERIT;\n",
    );

    assert_eq!(session.failed(), 0, "{}", session.messages());
    // **口令不许回显**:引擎会把语句原文打出来
    assert!(!session.messages().contains("hunter2"), "{}", session.messages());
    assert!(session.messages().contains("PASSWORD '***'"), "{}", session.messages());

    let store = service.users().expect("读用户表");
    let reader = store.find("reader").expect("reader 建出来了");
    assert!(!reader.can_login, "CREATE ROLE 默认 NOLOGIN");
    let carol = store.find("carol").expect("carol 建出来了");
    assert!(carol.can_login, "CREATE USER 默认 LOGIN");
    assert_eq!(carol.member_of, ["reader"]);
    assert!(store.authenticate("carol", "hunter2").is_ok());
}

#[test]
fn a_plain_user_cannot_manage_roles_or_create_databases() {
    let dir = TempDir::new("no-authority");
    let service = dir.service();
    add(&service, "bob", RoleAttrs::bootstrap());
    let alice = add(&service, "alice", attrs(false, true));
    service.create_database("alice", "alice").unwrap();

    let mut session = open(&service, "alice", &alice);
    session.run_script("CREATE ROLE r;");
    assert!(session.messages().contains("CREATEROLE"), "{}", session.messages());
    session.run_script("CREATE DATABASE other;");
    assert!(session.messages().contains("CREATEDB"), "{}", session.messages());
    assert!(!service.exists("other"), "拒绝的语句不该建出库来");

    // 自己的口令可以自己换;换成别人的不行
    session.run_script("ALTER USER alice PASSWORD 'new-pw';");
    assert_eq!(session.failed(), 2, "{}", session.messages());
    assert!(service.users().unwrap().authenticate("alice", "new-pw").is_ok());

    session.run_script("ALTER USER bob PASSWORD 'hacked';");
    assert!(session.messages().contains("CREATEROLE"), "{}", session.messages());
    assert!(service.users().unwrap().authenticate("bob", "pw").is_ok(), "bob 的口令不该被动");
}

#[test]
fn set_role_switches_identity_and_rollback_puts_it_back() {
    let dir = TempDir::new("set-role");
    let service = dir.service();
    let bob = add(&service, "bob", RoleAttrs::bootstrap());
    service.create_database("bob", "bob").unwrap();

    let mut session = open(&service, "bob", &bob);
    assert_eq!(session.symbols().role(), "bob");

    session.run_script("CREATE ROLE r1; CREATE ROLE r2; GRANT r1 TO r2;");
    assert_eq!(session.failed(), 0, "{}", session.messages());

    // 超级用户能借任何身份
    session.run_script("SET ROLE r2;");
    assert_eq!(session.symbols().role(), "r2");
    assert!(session.messages().contains("当前角色:r2"), "{}", session.messages());

    // r2 是 r1 的成员
    session.run_script("SET ROLE r1;");
    assert_eq!(session.symbols().role(), "r1");

    session.run_script("SET ROLE NONE;");
    assert_eq!(session.symbols().role(), "bob");
    session.run_script("SET ROLE r2; RESET ROLE;");
    assert_eq!(session.symbols().role(), "bob");

    // 没有的角色:报错,身份不变
    session.run_script("SET ROLE nobody;");
    assert_eq!(session.symbols().role(), "bob");
    assert!(session.messages().contains("没有角色"), "{}", session.messages());

    // 事务回滚把角色盖回去(当前角色是会话状态,与 search_path 同一档)
    session.run_script("BEGIN; SET ROLE r1;");
    assert_eq!(session.symbols().role(), "r1");
    session.run_script("ROLLBACK;");
    assert_eq!(session.symbols().role(), "bob");

    session.run_script("BEGIN; SET ROLE r1; COMMIT;");
    assert_eq!(session.symbols().role(), "r1", "COMMIT 之后留着");
}

#[test]
fn a_non_superuser_can_only_set_role_to_a_role_it_belongs_to() {
    let dir = TempDir::new("set-role-member");
    let service = dir.service();
    add(&service, "root", RoleAttrs::bootstrap());
    let alice = add(&service, "alice", attrs(false, true));
    service.create_database("alice", "alice").unwrap();
    service.create_role("r1", None, RoleAttrs::role(), &[]).unwrap();
    service.create_role("r2", None, RoleAttrs::role(), &[]).unwrap();
    service
        .grant_roles(&["r1".to_string()], &["alice".to_string()], true)
        .unwrap();

    let mut session = open(&service, "alice", &alice);
    session.run_script("SET ROLE r1;");
    assert_eq!(session.symbols().role(), "r1", "{}", session.messages());

    session.run_script("SET ROLE r2;");
    assert_eq!(session.symbols().role(), "r1", "借不到的身份不该改身份");
    assert!(session.messages().contains("不是"), "{}", session.messages());
}

#[test]
fn inheriting_the_owner_role_lets_a_member_connect_and_do_ddl() {
    let dir = TempDir::new("inherited-owner");
    let service = dir.service();
    add(&service, "team", attrs(false, false)); // owner:NOLOGIN
    add(&service, "member", attrs(false, true));
    let loner = add(&service, "loner", {
        let mut a = attrs(false, true);
        a.inherit = false;
        a
    });
    service.create_database("teamdb", "team").unwrap();
    service
        .grant_roles(&["team".to_string()], &["member".to_string()], true)
        .unwrap();

    // 继承来的 owner:连得上,也能动结构
    let member = service.users().unwrap().find("member").unwrap().clone();
    let mut session = open(&service, "teamdb", &member);
    session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1);");
    assert_eq!(session.failed(), 0, "{}", session.messages());
    drop(session);

    // NOINHERIT 的那个:是成员,但拿不到 owner 的权限
    let err = service
        .open("teamdb", &loner)
        .expect_err("NOINHERIT 不该拿到 owner 的权限");
    assert!(err.to_string().contains("loner"), "{err}");
}

// ------------------------------------------------------------------ 内存模式

#[test]
fn a_memory_session_refuses_role_statements_but_still_runs_sql() {
    let mut session = Session::with_sinks(Sink::buffer(), Sink::buffer());

    session.run_script("CREATE ROLE r; SET ROLE r;");
    assert!(session.messages().contains("没有服务目录"), "{}", session.messages());
    assert_eq!(session.failed(), 2);

    // 表结构那几条照旧(内存里没有用户,权限检查放行)
    session.run_script("CREATE TABLE t (x INT); INSERT INTO t VALUES (1); SELECT x FROM t;");
    assert_eq!(session.failed(), 2, "{}", session.messages());
    assert!(session.query_output().contains('1'), "{}", session.query_output());
}

// ------------------------------------------------------------------ 事务里

#[test]
fn role_statements_are_refused_inside_a_transaction() {
    let dir = TempDir::new("roles-tx");
    let service = dir.service();
    let bob = add(&service, "bob", RoleAttrs::bootstrap());
    service.create_database("bob", "bob").unwrap();

    let mut session = open(&service, "bob", &bob);
    for sql in [
        "BEGIN; CREATE ROLE r;",
        "BEGIN; ALTER ROLE bob INHERIT;",
        "BEGIN; DROP ROLE r2;",
        "BEGIN; GRANT r TO bob;",
    ] {
        session.run_script(sql);
        assert!(session.messages().contains("不能在事务里跑"), "{sql}:{}", session.messages());
        session.run_script("ROLLBACK;");
    }
}

#[test]
fn renaming_and_dropping_roles_from_a_session() {
    let dir = TempDir::new("session-rename");
    let service = dir.service();
    let bob = add(&service, "bob", RoleAttrs::bootstrap());
    service.create_database("bob", "bob").unwrap();

    let mut session = open(&service, "bob", &bob);
    session.run_script(
        "CREATE ROLE r1;\n\
         CREATE ROLE r2;\n\
         ALTER ROLE r1 RENAME TO reader;\n\
         GRANT reader TO r2;\n\
         DROP ROLE r2;\n\
         CREATE USER temp PASSWORD 'temp-pw';\n\
         ALTER USER temp PASSWORD 'another';\n\
         DROP USER temp;",
    );
    assert_eq!(session.failed(), 0, "{}", session.messages());
    assert!(!session.messages().contains("temp-pw"), "{}", session.messages());
    assert!(!session.messages().contains("another"), "{}", session.messages());

    let store = service.users().expect("读用户表");
    assert!(store.find("r1").is_none() && store.find("r2").is_none());
    assert!(store.find("reader").is_some());
    assert!(store.find("temp").is_none());

    // 当前会话用户不能改名,也不能删(会话里存的就是名字)
    session.run_script("ALTER ROLE bob RENAME TO boss;");
    assert!(session.messages().contains("不能改"), "{}", session.messages());
    session.run_script("DROP ROLE bob;");
    assert!(session.messages().contains("不能删"), "{}", session.messages());
    let store = service.users().expect("读用户表");
    assert!(store.find("bob").is_some() && store.find("boss").is_none());
}
