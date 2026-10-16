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

/// `ysql-csv` 的二进制:与 `ysql` 在同一个输出目录里,只是名字不同.
///
/// 用"兄弟文件"推而不是 `env!("CARGO_BIN_EXE_ysql-csv")`:环境变量名里带横线在
/// 有些 shell 下不好使,而这两个目标本来就由**同一次** `cargo build` 产出.
fn csv_bin() -> PathBuf {
    PathBuf::from(bin()).with_file_name("ysql-csv")
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
    run_with_lang(dir, args, password, input, "zh-CN")
}

/// 同 [`run_with_stdin`],但显式指定语言.
///
/// 上面那几层钉的是 `zh-CN`(断言写的是中文);要验**机器可读**的那几行
/// (`summarize.py` 认的 `Time: x ms`)就得切到 `en`--`ydbms-test/ydbms.sh`
/// 也是这么干的.
fn run_with_lang(
    dir: &TempDir,
    args: &[&str],
    password: Option<&str>,
    input: Option<&str>,
    lang: &str,
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
        .env("YDBMS_LANG", lang);

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

#[test]
fn a_csv_table_is_plain_csv_on_disk_and_readable_by_the_heap_binary() {
    // 存储模式两个入口:`ysql --storage csv` 与 `ysql-csv`(预设).两者造的都是一份
    // **标准 CSV**;而默认(堆)那一支照样读得到它--格式跟着表走,两种可以同库并存.
    let dir = TempDir::new("storage-csv");
    bootstrap(&dir);

    let create = dir.script(
        "create.sql",
        "CREATE TABLE t (x INT, note TEXT);\nINSERT INTO t VALUES (1, 'a'), (2, 'b');\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "bob", "--storage", "csv", "-f", create.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let lib = dir.path().join("bob");
    assert_eq!(
        std::fs::read_to_string(lib.join("t.csv")).expect("t.csv 该在"),
        "x,note\n1,a\n2,b\n",
        "CSV 表就是标准 CSV"
    );
    assert!(!lib.join("t.heap").exists(), "不该落堆文件");

    // 默认(堆)那一支读得回来
    let query = dir.script("query.sql", "SELECT note FROM t ORDER BY x;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains('a') && text.contains('b'), "{text}");

    // 再用 `ysql-csv` 那一支建第二张表:同一次构建产出的另一个二进制
    assert!(csv_bin().is_file(), "同一次 cargo build 该产出 {} ", csv_bin().display());
    let mut command = Command::new(csv_bin());
    command
        .arg("--server-dist")
        .arg(dir.path())
        .args(["-U", "bob", "-d", "bob", "-f"])
        .arg(dir.script("second.sql", "CREATE TABLE u (y INT);\nINSERT INTO u VALUES (9);\n"))
        .env_remove("PGPASSWORD")
        .env("PGPASSWORD", PASSWORD)
        .env("YDBMS_LANG", "zh-CN")
        .stdin(Stdio::null());
    let out = command.output().expect("起不了 ysql-csv");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(lib.join("u.csv").is_file(), "ysql-csv 建的表也是 CSV");
    assert_eq!(
        std::fs::read_to_string(lib.join("u.csv")).unwrap(),
        "y\n9\n"
    );
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

    // 1) bootstrap + 建库(连的是默认库 `bob`).
    // 脚本模式默认安静,这里想看状态行,所以第一句把 VERBOSE 打开(顺带覆盖大写取值).
    let create = dir.script("create.sql", "\\set VERBOSE ON\nCREATE DATABASE filmdb;\n");
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
        "\\set VERBOSE ON\n\
         CREATE TABLE movies (movie_id INT, title VARCHAR(20));\n\
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

    // 磁盘上真有那些东西:数据在**页式堆文件**里(TODO 7.5),模式在旁路 JSON 里
    let lib = dir.path().join("filmdb");
    assert!(lib.join("movies.heap").is_file());
    assert!(lib.join("movies.schema.json").is_file());
    assert!(lib.join("catalog.json").is_file());
    let heap = std::fs::read(lib.join("movies.heap")).unwrap();
    assert!(heap.windows(5).any(|w| w == b"Alien"), "堆文件里该有那一行");
}

#[test]
fn a_database_level_search_path_survives_into_another_process() {
    // 5.4 的验收:一个进程里 `ALTER DATABASE ... SET search_path`,**另一个进程**
    // 里非限定名要能解析到 `study.movies`
    let dir = TempDir::new("db-settings");
    bootstrap(&dir);

    let setup = dir.script(
        "setup.sql",
        "\\set VERBOSE ON\n\
         CREATE SCHEMA study;\n\
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
    // 脚本模式安静,所以这里只有结果表:表头是 `movie_id`(说明非限定名解析到了
    // `study.movies`),数据是那一行
    let text = stdout(&out);
    assert!(text.contains("movie_id") && text.contains('1'), "{text}");

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

    // 提交完不留暂存残渣(提交记录 `manifest.json` 是**长期**在的:
    // 它记的是"磁盘上现在是哪一份",见 `usr::service` 的 `Manifest`)
    assert!(dir.path().join("bob/manifest.json").is_file());
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

    // `-f` 一路(脚本模式:**默认安静**,所以这里只有结果表)
    let by_flag = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert!(by_flag.status.success(), "{}", stderr(&by_flag));
    let by_flag_text = stdout(&by_flag);
    assert!(by_flag_text.contains("one\n---\n1\n"), "{by_flag_text}");
    assert!(!by_flag_text.contains("成功"), "脚本模式默认不打状态行:{by_flag_text}");

    // 交互终端(这里没有 tty,走的是 `ysql < 脚本` 那一路)里 `\i` 同一份脚本.
    // 两边**不再是逐字一致**:`-f` 默认安静,`\i` 默认不安静,所以是包含关系.
    let by_include = run_with_stdin(
        &dir,
        &["-U", "bob", "-d", "bob"],
        Some(PASSWORD),
        Some(&format!("\\i {}\n", query.display())),
    );
    assert!(by_include.status.success(), "{}", stderr(&by_include));
    let by_include_text = stdout(&by_include);
    assert!(by_include_text.contains("成功 1 条"), "{by_include_text}");

    assert!(
        by_include_text.contains(&by_flag_text),
        "`\\i` 的输出应当把 `-f` 的整段包在里面:\n--- -f ---\n{by_flag_text}\n--- \\i ---\n{by_include_text}"
    );
}

/// 脚本模式(`-f`)默认**安静**:状态行与汇总都不打,只有结果表;脚本里
/// `\set VERBOSE ON` 能把它打开.
#[test]
fn script_mode_is_quiet_until_verbose_is_turned_on() {
    let dir = TempDir::new("script-verbose");
    bootstrap(&dir);

    let quiet = dir.script(
        "quiet.sql",
        "CREATE TABLE t (x INT);\nINSERT INTO t VALUES (1);\nSELECT x FROM t;\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", quiet.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let text = stdout(&out);
    assert!(text.contains("x\n-\n1\n"), "结果表照旧:{text}");
    for needle in ["建表成功", "插入 1 行", "成功 1 条", "读入"] {
        assert!(!text.contains(needle), "脚本模式默认不该有 `{needle}`:{text}");
    }

    let loud = dir.script(
        "loud.sql",
        "\\set VERBOSE ON\nCREATE TABLE u (x INT);\nINSERT INTO u VALUES (2);\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", loud.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let text = stdout(&out);
    assert!(text.contains("建表成功:u"), "{text}");
    assert!(text.contains("插入 1 行到 u"), "{text}");
    assert!(text.contains("成功 2 条,失败 0 条"), "{text}");
    // 打开的那一句自己也要报,而且**读入**是开关打开**之前**打的,所以仍然没有
    assert!(text.contains("VERBOSE:开"), "{text}");
    assert!(!text.contains("读入"), "读入发生在开关打开之前:{text}");
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

// ------------------------------------------------------------ 多用户共享(6.4)

/// 验收原文:**表级 `GRANT SELECT` 之后另一个人 `SELECT` 得过,`INSERT` 被拒**.
///
/// 这里"另一个人"是**另一个真进程**(`-U alice`),授权与口令轮换也顺带走一遍.
#[test]
fn a_granted_user_reads_but_cannot_write_across_processes() {
    let dir = TempDir::new("share");
    bootstrap(&dir);

    // bob(超级用户)建库 filmdb 并装一张表,再把 alice 建出来并授权
    let setup = dir.script(
        "setup.sql",
        "CREATE DATABASE filmdb;\n",
    );
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", setup.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let setup = dir.script(
        "load.sql",
        "CREATE TABLE movies (movie_id INT, title TEXT);\n\
         INSERT INTO movies VALUES (1, 'Alien'), (2, 'Aliens');\n\
         CREATE USER alice PASSWORD 'alice-pw';\n\
         GRANT CONNECT ON DATABASE filmdb TO alice;\n\
         GRANT SELECT (title) ON movies TO alice;\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "filmdb", "-f", setup.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        !stdout(&out).contains("alice-pw"),
        "口令不许回显:{}",
        stdout(&out)
    );

    // alice:读得到(title 是授过的那一列)
    let query = dir.script("query.sql", "SELECT title FROM movies ORDER BY title;\n");
    let out = run(
        &dir,
        &["-U", "alice", "-d", "filmdb", "-f", query.to_str().unwrap()],
        Some("alice-pw"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("Alien"), "{}", stdout(&out));

    // alice:没授的列读不了
    let query = dir.script("other-column.sql", "SELECT movie_id FROM movies;\n");
    let out = run(
        &dir,
        &["-U", "alice", "-d", "filmdb", "-f", query.to_str().unwrap()],
        Some("alice-pw"),
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("SELECT"), "{}", stdout(&out));

    // alice:写不了
    let insert = dir.script(
        "insert.sql",
        "INSERT INTO movies (title) VALUES ('Alien3');\n",
    );
    let out = run(
        &dir,
        &["-U", "alice", "-d", "filmdb", "-f", insert.to_str().unwrap()],
        Some("alice-pw"),
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("INSERT"), "{}", stdout(&out));

    // 口令轮换:bob 换掉 alice 的口令,旧口令就不认了
    let rotate = dir.script(
        "rotate.sql",
        "ALTER USER alice PASSWORD 'new-pw';\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "filmdb", "-f", rotate.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(!stdout(&out).contains("new-pw"), "{}", stdout(&out));

    let old = run(
        &dir,
        &["-U", "alice", "-d", "filmdb", "-f", query.to_str().unwrap()],
        Some("alice-pw"),
    );
    assert_eq!(old.status.code(), Some(1));
    assert!(stderr(&old).contains("口令不对"), "{}", stderr(&old));

    // 数据一行都没多
    let check = dir.script("check.sql", "SELECT title FROM movies ORDER BY title;\n");
    let out = run(
        &dir,
        &["-U", "alice", "-d", "filmdb", "-f", check.to_str().unwrap()],
        Some("new-pw"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(stdout(&out).matches("Aliens").count(), 1, "{}", stdout(&out));
    assert!(!stdout(&out).contains("Alien3"), "{}", stdout(&out));

    // 授权表就在库目录里
    assert!(dir.path().join("filmdb/grants.json").is_file());
}

/// 验收原文:**`\c filmdb` 之后旧库的锁已释放(另一个进程能连上)**.
///
/// 这里两个都是"另一个进程":子进程是交互式的(管道喂输入),它 `\c other` 之后,
/// **测试进程**必须马上能连上它放手了的那个库.
#[test]
fn switching_database_releases_the_old_lock_across_processes() {
    use std::io::{BufRead, Write as _};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    let dir = TempDir::new("switch-db");
    bootstrap(&dir);

    // 先建出第二个库(用一次性进程)
    let setup = dir.script("setup.sql", "CREATE DATABASE other;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", setup.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // 交互式子进程(标准输入是管道:读完一段就走非 tty 那条路)
    let mut child = Command::new(bin())
        .arg("--server-dist")
        .arg(dir.path())
        .args(["-U", "bob", "-d", "bob"])
        .env_remove("PGPASSWORD")
        .env("PGPASSWORD", PASSWORD)
        .env("YDBMS_LANG", "zh-CN")
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

    // 等横幅(= 库已经打开)
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

    // 让它换库
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"\\c other\n")
        .expect("喂输入");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains("已连到库") => break,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            Err(RecvTimeoutError::Timeout) => panic!("等不到子进程换库"),
            Err(RecvTimeoutError::Disconnected) => panic!("子进程换库时退出了"),
        }
    }

    // 旧库的锁已经放了:测试进程立刻连得上
    let service = ServiceDir::resolve(Some(dir.path().to_path_buf()), None).expect("服务目录");
    let freed = service.open("bob", &bob()).expect("旧库的锁该放了");

    // 而它现在握着的那个库,别人连不上
    let err = service.open("other", &bob()).expect_err("新库还在子进程手里");
    assert!(err.to_string().contains("单写者锁"), "{err}");

    drop(freed);
    let _ = child.kill();
    let _ = child.wait();
}

/// `\c` 在**脚本**里也能用(`-f` 与 `\i` 是同一条路):后半段跑在换过去的库里.
#[test]
fn a_script_may_switch_databases() {
    let dir = TempDir::new("script-switch");
    bootstrap(&dir);

    let setup = dir.script(
        "setup.sql",
        "CREATE DATABASE other;\n",
    );
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", setup.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let load = dir.script(
        "load.sql",
        "CREATE TABLE t (x INT);\nINSERT INTO t VALUES (42);\n",
    );
    let out = run(
        &dir,
        &["-U", "bob", "-d", "other", "-f", load.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // 从默认库出发,`\c other` 之后读到的是 other 里的表
    let script = dir.script("switch.sql", "\\c other\nSELECT x FROM t;\n");
    let out = run(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", script.to_str().unwrap()],
        Some(PASSWORD),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("已连到库"), "{}", stdout(&out));
    assert!(stdout(&out).contains("42"), "{}", stdout(&out));

    // 反过来:在 `bob` 库里没有 t
    let query = dir.script("q.sql", "SELECT x FROM t;\n");
    let out = run(&dir, &["-U", "bob", "-d", "bob", "-f", query.to_str().unwrap()], Some(PASSWORD));
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
}

// ------------------------------------------- 相对 `--server-dist` 与 `\cd`

/// 在**指定的工作目录**里跑一次 `ysql`:`--server-dist` 由调用方原样给出(要测的正是
/// 相对路径),标准输入给 `input`.
fn run_in(cwd: &Path, dist: &str, args: &[&str], input: &str) -> Output {
    let mut command = Command::new(bin());
    command
        .current_dir(cwd)
        .arg("--server-dist")
        .arg(dist)
        .args(args)
        .env_remove("PGPASSWORD")
        .env("PGPASSWORD", PASSWORD)
        .env("YDBMS_LANG", "zh-CN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("起不了 ysql");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("喂输入");
    child.wait_with_output().expect("收输出")
}

/// `--server-dist` 给的是**相对路径**时,它在启动那一刻就按当时的工作目录定死:
/// 之后 `\cd` 换了目录,服务目录还在原地--库不跟着搬走.
///
/// 这是"库看起来搬了家"的回归测试:服务目录要是留着相对形式,`\cd` 之后 `join` 出来
/// 的表文件会写到新工作目录下面,`catalog.json` 却留在老地方,同一个库就此分家.
#[test]
fn a_relative_server_dist_does_not_move_with_cd() {
    let dir = TempDir::new("relative-dist");
    // 调用时的工作目录是 base,服务目录给它的子目录 `srv`(相对路径)
    let base = dir.path().join("base");
    let srv = base.join("srv");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&srv).expect("建 base/srv");
    std::fs::create_dir_all(&elsewhere).expect("建 elsewhere");
    let empty = dir.script("empty.sql", "");

    let out = run_in(&base, "srv", &["-U", "bob", "-f", empty.to_str().unwrap()], "");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(srv.join("users.json").is_file(), "服务目录该落在 base/srv");

    // `\cd` 出去之后建表写数据
    let input = format!(
        "\\cd {}\nCREATE TABLE t (a int);\nINSERT INTO t VALUES (7);\n",
        elsewhere.display()
    );
    let out = run_in(&base, "srv", &["-U", "bob"], &input);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        !elsewhere.join("srv").exists(),
        "服务目录跟着 `\\cd` 搬到了 {}:{}",
        elsewhere.display(),
        stdout(&out)
    );

    // 表文件与 `catalog.json` 在同一个库目录里,再起一个进程也读得回来
    assert!(srv.join("bob/t.heap").is_file(), "表文件该在 base/srv/bob");
    let heap = std::fs::read(srv.join("bob/t.heap")).expect("堆文件该在 base/srv/bob");
    assert!(heap.windows(1).any(|w| w == b"7"), "堆文件里该有那一行");
    let out = run_in(&base, "srv", &["-U", "bob"], "SELECT a FROM t;\n");
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains('7'), "{}", stdout(&out));
}

// ------------------------------------------------ 8.9:命令标签(psql 协议)

/// 写基准那两行是**机器可读**的:根目录的 `summarize.py` 按
/// `^(?:INSERT 0|UPDATE) (\d+)$` 认"写了多少行",按一行裸 `COMMIT` 认提交,
/// 按 `^Time:\s*([\d.]+)\s*ms\b` 认耗时.这条测试钉的就是这三件事.
///
/// 注意它跑的是 `-f`(脚本模式),`VERBOSE` 默认关:状态行一条都没有,标签照常打.
#[test]
fn the_write_benchmark_lines_are_psql_shaped() {
    let dir = TempDir::new("tags");
    bootstrap(&dir);

    let script = dir.script(
        "write.sql",
        "\\timing on\n\
         BEGIN;\n\
         CREATE TABLE t (x INT);\n\
         INSERT INTO t VALUES (1), (2);\n\
         UPDATE t SET x = x + 1;\n\
         COMMIT;\n",
    );
    let out = run_with_lang(
        &dir,
        &["-U", "bob", "-d", "bob", "-f", script.to_str().unwrap()],
        Some(PASSWORD),
        None,
        // `summarize.py` 认的是**英文**那两行(`ydbms-test/ydbms.sh` 也这么设)
        "en",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();

    // 单独成行,没有前缀也没有后缀
    assert!(lines.contains(&"INSERT 0 2"), "{text}");
    assert!(lines.contains(&"UPDATE 2"), "{text}");
    assert!(lines.contains(&"COMMIT"), "{text}");
    // `-f` 默认安静:标签是唯一的状态输出(没有任何人类可读的状态行)
    assert!(!text.contains("插入"), "{text}");
    assert!(!text.contains("-> "), "{text}");

    // 紧跟在写标签后面的那一行就是耗时,形状照 psql
    let at = lines
        .iter()
        .position(|line| *line == "INSERT 0 2")
        .expect("有 INSERT 标签");
    let time = lines[at + 1];
    assert!(time.starts_with("Time: ") && time.ends_with(" ms"), "`{time}` 不像 psql 的耗时行:{text}");
    // 数值部分要能被正则吃掉(`[\d.]+`)
    let number = time
        .trim_start_matches("Time: ")
        .trim_end_matches(" ms")
        .to_string();
    assert!(
        !number.is_empty() && number.chars().all(|c| c.is_ascii_digit() || c == '.'),
        "`{number}` 不是 `[\\d.]+`:{text}"
    );
}
