//! `ysql` 的跨进程端到端 testbench(阶段 6.3).
//!
//! 每个用例一个临时**服务目录**,若干次**真进程**调用(`CARGO_BIN_EXE_ysql`).
//! 要验的就是这一句:**再起一个进程查回来**--数据在磁盘上,不是某个进程里的内存.
//!
//! 这里不碰 `~/.ydbms`:每一次调用都显式给 `--server-dist`,测试之间互不打扰.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::usr::{ServiceDir, User};

/// 测试里用的口令(**任何一个用例都不该在 `users.json` 里看到它**).
const PASSWORD: &str = "s3cret";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `ysql` 的二进制--cargo 为 `src/bin/ysql.rs` 生成的那个.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ysql")
}

/// 自清理的临时目录(每个用例一个**服务目录**).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-ysql-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 写一个脚本文件,返回它的路径.
    fn script(&self, name: &str, sql: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, sql).expect("写脚本");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 跑一次 `ysql`:`--server-dist` 指向临时目录,标准输入给 null(非交互).
fn run(dir: &TempDir, args: &[&str], password: Option<&str>) -> Output {
    run_with_stdin(dir, args, password, None)
}

/// 同 [`run`],但可以从管道喂输入(`ysql < 脚本.sql` 那一路).
fn run_with_stdin(
    dir: &TempDir,
    args: &[&str],
    password: Option<&str>,
    input: Option<&str>,
) -> Output {
    let mut command = Command::new(bin());
    command
        .arg("--server-dist")
        .arg(dir.path())
        .args(args)
        // 环境里的 PGPASSWORD 不能影响测试(本机可能正好设着)
        .env_remove("PGPASSWORD")
        // 孩子进程报的是中文(默认就是),但这儿显式钉住:断言写的是中文,
        // 不能让开发机的 LANG 决定它过不过
        .env("YDBMS_LANG", "zh-CN");

    if let Some(password) = password {
        command.env("PGPASSWORD", password);
    }

    match input {
        Some(text) => {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().expect("起不了 ysql");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(text.as_bytes())
                .expect("喂输入");
            child.wait_with_output().expect("收输出")
        }
        None => {
            command.stdin(Stdio::null());
            command.output().expect("起不了 ysql")
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// 第一次跑(bootstrap):`users.json` 与默认库都建出来.
fn bootstrap(dir: &TempDir) {
    let empty = dir.script("empty.sql", "");
    let out = run(dir, &["-U", "bob", "-f", empty.to_str().unwrap()], Some(PASSWORD));
    assert!(out.status.success(), "bootstrap 失败:{}", stderr(&out));
    assert!(dir.path().join("users.json").is_file());
    assert!(dir.path().join("bob/catalog.json").is_file(), "默认库一并建出来");
}

// ------------------------------------------------------------------ 参数

#[test]
fn help_exits_zero_and_lists_the_options() {
    let dir = TempDir::new("help");
    let out = run(&dir, &["--help"], None);

    assert!(out.status.success(), "{}", stderr(&out));
    for needle in ["-U", "-d", "-f", "--server-dist", "$HOME/.ydbms"] {
        assert!(stdout(&out).contains(needle), "帮助里少了 `{needle}`");
    }
    // 只是看帮助,不该把服务目录建出来
    assert!(!dir.path().join("users.json").exists());
}

#[test]
fn an_unknown_argument_exits_non_zero() {
    let dir = TempDir::new("bad-arg");
    let out = run(&dir, &["--nope"], None);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("看不懂的参数"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "报错不该往 stdout 写东西");
}

#[test]
fn a_missing_password_in_a_non_interactive_run_is_refused() {
    let dir = TempDir::new("no-password");
    let out = run(&dir, &["-U", "bob"], None);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("PGPASSWORD"), "{}", stderr(&out));
    assert!(
        !dir.path().join("users.json").exists(),
        "没认证成功就不该落下任何东西"
    );
}

#[test]
fn an_unknown_user_is_refused() {
    let dir = TempDir::new("unknown-user");
    bootstrap(&dir);

    let out = run(&dir, &["-U", "alice", "-d", "bob"], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("不存在"), "{}", stderr(&out));
}

#[test]
fn a_wrong_password_exits_non_zero() {
    let dir = TempDir::new("wrong-password");
    bootstrap(&dir);

    let out = run(&dir, &["-U", "bob", "-d", "bob"], Some("s3cre"));
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("口令不对"), "{}", stderr(&out));
}

// ------------------------------------------------------------ 跨进程

#[test]
fn a_database_and_its_rows_survive_across_processes() {
    let dir = TempDir::new("cross-process");

    // 1) bootstrap + 建库(连的是默认库 `bob`)
    let create = dir.script("create.sql", "CREATE DATABASE filmdb;\n");
    let out = run(&dir, &["-U", "bob", "-f", create.to_str().unwrap()], Some(PASSWORD));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("库 filmdb 已建(owner bob)"),
        "{}",
        stdout(&out)
    );
    assert!(stderr(&out).contains("首次使用"), "{}", stderr(&out));

    // 2) 另一个进程:连 filmdb 装数据
    let load = dir.script(
        "load.sql",
        "CREATE TABLE movies (movie_id INT, title VARCHAR(20));\n\
         INSERT INTO movies VALUES (1, 'Alien'), (2, 'Aliens');\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "filmdb", "-f", load.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("插入 2 行"), "{}", stdout(&out));

    // 3) 第三个进程:只查,不装
    let query = dir.script("query.sql", "SELECT movie_id, title FROM movies ORDER BY movie_id;\n");
    let out = run(
        &dir,
        &["-U", "bob", "-d", "filmdb", "-f", query.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("1 | Alien"), "{text}");
    assert!(text.contains("2 | Aliens"), "{text}");

    // 磁盘上真有那些东西
    let lib = dir.path().join("filmdb");
    assert!(lib.join("movies.csv").is_file());
    assert!(lib.join("movies.schema.json").is_file());
    assert!(lib.join("catalog.json").is_file());
    let csv = std::fs::read_to_string(lib.join("movies.csv")).unwrap();
    assert!(csv.contains("Alien"), "{csv}");
}

#[test]
fn a_database_level_search_path_survives_into_another_process() {
    // 5.4 的验收:一个进程里 `ALTER DATABASE ... SET search_path`,**另一个进程**
    // 里非限定名要能解析到 `study.movies`
    let dir = TempDir::new("db-settings");
    bootstrap(&dir);

    let setup = dir.script(
        "setup.sql",
        "CREATE SCHEMA study;\n\
         CREATE TABLE study.movies (movie_id INT, title VARCHAR(20));\n\
         INSERT INTO study.movies VALUES (1, 'Alien');\n\
         ALTER DATABASE bob SET search_path TO study;\n",
    );
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", setup.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("只影响以后的连接"), "{}", stdout(&out));

    // 另一个进程:非限定名 `movies`
    let query = dir.script("query.sql", "SELECT movie_id FROM movies;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("1 | Alien") || stdout(&out).contains(" 1"), "{}", stdout(&out));

    // 落盘的是 `catalog.json` 里的 `settings`
    let json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("bob/catalog.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(json["settings"]["search_path"][0].as_str(), Some("study"));
}

#[test]
fn a_transaction_across_a_script_commits_once_at_the_end() {
    // `BEGIN; ... COMMIT;` 包起来的脚本:中途一次都不落盘,`COMMIT` 才写
    let dir = TempDir::new("tx-script");
    bootstrap(&dir);

    let load = dir.script(
        "load.sql",
        "BEGIN;\n\
         CREATE TABLE t (x INT);\n\
         INSERT INTO t VALUES (1), (2);\n\
         COMMIT;\n",
    );
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", load.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // 提交之后换一个进程读回来
    let query = dir.script("query.sql", "SELECT x FROM t ORDER BY x;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains('1') && text.contains('2'), "{text}");

    // 提交意图用完就删掉,不留垃圾
    assert!(!dir.path().join("bob/commit.json").exists());
    assert!(!dir.path().join("bob/t.csv.new").exists());
}

#[test]
fn a_script_that_fails_makes_the_process_exit_non_zero() {
    let dir = TempDir::new("script-failed");
    bootstrap(&dir);

    let bad = dir.script("bad.sql", "SELECT * FROM nope;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", bad.to_str().unwrap()], Some(PASSWORD));

    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("unknown symbol"), "{}", stdout(&out));

    // 没报错的脚本照旧 0
    let ok = dir.script("ok.sql", "SELECT 1 AS one;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", ok.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn an_explicit_database_that_does_not_exist_is_refused_with_the_list() {
    let dir = TempDir::new("no-db");
    bootstrap(&dir);

    let query = dir.script("q.sql", "SELECT 1;\n");
    let out = run(
        &dir,
        &["-U", "bob", "-d", "nope", "-f", query.to_str().unwrap()],
        Some(PASSWORD),
    );

    assert_eq!(out.status.code(), Some(1));
    let text = stderr(&out);
    assert!(text.contains("库 `nope` 不存在"), "{text}");
    assert!(text.contains("bob"), "要列出已有的库:{text}");
    assert!(!dir.path().join("nope").exists(), "报错就不该建它");
}

#[test]
fn script_mode_and_backslash_i_produce_the_same_result() {
    let dir = TempDir::new("same-result");
    bootstrap(&dir);

    let query = dir.script("q.sql", "SELECT 1 AS one;\n");

    // `-f` 一路
    let by_flag = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert!(by_flag.status.success(), "{}", stderr(&by_flag));
    let by_flag_text = stdout(&by_flag);
    assert!(by_flag_text.contains("one\n---\n1\n"), "{by_flag_text}");

    // 交互终端(这里没有 tty,走的是 `ysql < 脚本` 那一路)里 `\i` 同一份脚本
    let by_include = run_with_stdin(
        &dir,
        &["-U", "bob", "-d", "bob"],
        Some(PASSWORD),
        Some(&format!("\\i {}\n", query.display())),
    );
    assert!(by_include.status.success(), "{}", stderr(&by_include));
    let by_include_text = stdout(&by_include);

    assert!(
        by_include_text.contains(&by_flag_text),
        "`\\i` 的输出应当把 `-f` 的整段包在里面:\n--- -f ---\n{by_flag_text}\n--- \\i ---\n{by_include_text}"
    );
}

/// 跨进程的单写者锁:库被这个测试进程握着时,真 `ysql` 进程必须连不上.
///
/// 这是 5.3 / 6.3 的验收口径之一("同一个库开两个 `ysql`,第二个明确报错"),
/// 同进程的两把句柄互斥在 `tests/usr_service.rs` 里另有一条.
#[test]
fn a_second_process_cannot_open_a_database_that_is_already_open() {
    let dir = TempDir::new("locked-across-processes");
    bootstrap(&dir);

    let service = ServiceDir::resolve(Some(dir.path().to_path_buf()), None).expect("服务目录");
    let held = service.open("bob", &bob()).expect("测试进程先把库打开");

    let query = dir.script("q.sql", "SELECT 1;\n");
    let out = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stderr(&out).contains("单写者锁"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "连不上就什么都不该跑:{}", stdout(&out));

    // 放手之后同一个库马上又能连(锁跟着句柄走,没有陈旧锁文件)
    drop(held);
    let out = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

/// 测试里造一个 `bob`(认证已经在 `users.json` 里过了,这里只是 owner 判定的入参).
fn bob() -> User {
    User {
        name: "bob".to_string(),
        salt: String::new(),
        password_hash: String::new(),
        is_admin: true,
        created_unix: 0,
        can_login: true,
        can_create_db: true,
        can_create_role: true,
        inherit: true,
        member_of: Vec::new(),
    }
}

/// `kill -9` 不留"陈旧锁":硬杀一个正开着库的进程之后,锁由内核释放,别的新进程
/// 马上能连(本版没有"锁文件里有 pid 就删掉"那种清理--那种清理才是危险的).
///
/// 判定"子进程已经把库打开"靠它的**横幅**:横幅是在 `Session::open`(含上锁)之后
/// 才打的.测试进程自己不去碰那把锁--`service.open` 会**抢**锁,一抢就把子进程坑死了.
#[test]
fn killing_a_session_releases_the_single_writer_lock() {
    use std::io::BufRead;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    let dir = TempDir::new("kill-lock");
    bootstrap(&dir);

    let mut child = Command::new(bin())
        .arg("--server-dist")
        .arg(dir.path())
        .args(["-U", "bob", "-d", "bob"])
        .env_remove("PGPASSWORD")
        .env("PGPASSWORD", PASSWORD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("起不了 ysql");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // 等横幅(= 库已经打开,锁已经在它手里)
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains("服务目录") => break,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            Err(RecvTimeoutError::Timeout) => panic!("等不到子进程把库打开"),
            Err(RecvTimeoutError::Disconnected) => panic!("子进程提前退出了"),
        }
    }

    // 硬杀:不给它走正常退出路径
    child.kill().expect("kill -9");
    let _ = child.wait();

    let service = ServiceDir::resolve(Some(dir.path().to_path_buf()), None).expect("服务目录");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(db) = service.open("bob", &bob()) {
            drop(db);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "kill -9 之后锁没被释放(留下了陈旧锁?)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn users_json_holds_no_plaintext_password() {
    let dir = TempDir::new("plaintext");
    bootstrap(&dir);

    let raw = std::fs::read_to_string(dir.path().join("users.json")).expect("读 users.json");
    assert!(!raw.contains(PASSWORD), "口令不能明文落盘:{raw}");
    assert!(raw.contains("password_hash"), "{raw}");

    // 目录树:users.json + <库>/catalog.json + 表文件(这里还没建表)
    assert!(dir.path().join("bob/catalog.json").is_file());
    assert!(dir.path().join("bob/.lock").is_file());
}
