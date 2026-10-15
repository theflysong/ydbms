//! 执行引擎与元命令的 testbench.
//!
//! 两路输出都攒在内存里,所以可以精确断言**哪一路**拿到了什么--这正是 `\o`
//! "仅限于查询输出"这句话能不能被验证的关键.
//!
//! `\cd` 改的是**进程级**工作目录,所以碰它的测试要拿一把锁串行,用完还原.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ydbms::cmd::{Session, Sink};

/// 改工作目录的测试之间互斥(`set_current_dir` 是进程级的).
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// 两路输出都进内存的会话.
fn buffered() -> Session {
    Session::with_sinks(Sink::buffer(), Sink::buffer())
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-session-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ------------------------------------------------------------ 简单元命令

#[test]
fn echo_writes_the_text_verbatim() {
    let mut s = buffered();
    s.run_interactive("\\echo 你好 世界");
    assert_eq!(s.messages(), "你好 世界\n");
}

#[test]
fn echo_drops_a_wrapping_pair_of_single_quotes() {
    // psql 的口径:`\echo 'Test 1: ...'` 打出来不带引号(`ydbms-test/` 那套输出靠它对齐)
    let mut s = buffered();
    s.run_interactive("\\echo 'Test 1: Baseline tests'");
    assert_eq!(s.messages(), "Test 1: Baseline tests\n");

    // 一对引号里再写 `''` 就是一个单引号
    s.run_interactive("\\echo 'it''s fine'");
    assert!(s.messages().ends_with("it's fine\n"), "{}", s.messages());

    // 空的那一对折成空行
    s.run_interactive("\\echo ''");
    assert!(s.messages().ends_with("it's fine\n\n"), "{}", s.messages());

    // **没被一对引号整个包住**就原样:psql 的逐参数去引号本版不做
    s.run_interactive("\\echo a 'b c'");
    assert!(s.messages().ends_with("a 'b c'\n"), "{}", s.messages());
}

#[test]
fn pwd_reports_the_working_directory() {
    let mut s = buffered();
    s.run_interactive("\\pwd");
    let expected = std::env::current_dir().unwrap().display().to_string();
    assert!(s.messages().contains(&expected), "{}", s.messages());
}

#[test]
fn ls_lists_a_directory_with_dirs_suffixed() {
    let dir = TempDir::new("ls");
    std::fs::write(dir.path().join("b.txt"), "x").unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let mut s = buffered();
    s.run_interactive(&format!("\\ls {}", dir.path().display()));

    let out = s.messages();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines, vec!["a.txt", "b.txt", "sub/"], "应当排序且目录带 /");
}

#[test]
fn ls_on_a_missing_directory_reports_but_does_not_break() {
    let mut s = buffered();
    s.run_interactive("\\ls /no/such/dir");
    assert!(s.messages().contains("读不了"), "{}", s.messages());

    // 会话仍然可用
    s.run_interactive("\\echo 还在");
    assert!(s.messages().contains("还在"));
}

#[test]
fn cd_changes_the_directory_and_empty_cd_goes_home() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let before = std::env::current_dir().unwrap();
    let dir = TempDir::new("cd");

    let mut s = buffered();
    s.run_interactive(&format!("\\cd {}", dir.path().display()));
    assert_eq!(std::env::current_dir().unwrap(), dir.path());

    s.run_interactive("\\cd");
    assert_ne!(std::env::current_dir().unwrap(), dir.path());

    std::env::set_current_dir(before).unwrap();
}

#[test]
fn cd_to_a_missing_directory_keeps_the_current_one() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let before = std::env::current_dir().unwrap();

    let mut s = buffered();
    s.run_interactive("\\cd /no/such/dir");
    assert!(s.messages().contains("切不到"), "{}", s.messages());
    assert_eq!(std::env::current_dir().unwrap(), before);
}

#[test]
fn unknown_metacommand_lists_the_known_ones() {
    let mut s = buffered();
    s.run_interactive("\\bogus");
    let out = s.messages();
    assert!(out.contains("未知元命令"), "{out}");
    assert!(out.contains("\\i"), "{out}");
}

// ------------------------------------------------------------ \d 与 \dt

#[test]
fn dt_lists_tables_with_counts() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE m (a INT, b INT)");
    s.run_interactive("INSERT INTO m VALUES (1, 2), (3, 4)");
    s.run_interactive("\\dt");

    let out = s.messages();
    assert!(out.contains("表"), "{out}");
    assert!(out.contains("m"), "{out}");
    assert!(out.contains('2'), "{out}");
}

#[test]
fn dt_on_an_empty_database_says_so() {
    let mut s = buffered();
    s.run_interactive("\\dt");
    assert!(s.messages().contains("没有任何表"), "{}", s.messages());
}

#[test]
fn describe_lists_the_columns_and_types() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE m (a VARCHAR(8), b TIMESTAMP, c NUMERIC(6,2))");
    s.run_interactive("\\d m");

    let out = s.messages();
    assert!(out.contains("VARCHAR(8)"), "{out}");
    assert!(out.contains("TIMESTAMP(6)"), "{out}");
    assert!(out.contains("NUMERIC(6,2)"), "{out}");
}

#[test]
fn describe_lists_the_indexes_of_the_table() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE m (a INT, b INT)");
    s.run_interactive("CREATE INDEX m_a ON m (a)");
    s.run_interactive("\\d m");

    let out = s.messages();
    assert!(out.contains("索引"), "{out}");
    assert!(out.contains("m_a"), "{out}");
    assert!(out.contains("(a)"), "{out}");

    // `DROP INDEX` 之后 `\d` 就不再报它
    s.run_interactive("DROP INDEX m_a");
    s.run_interactive("\\d m");
    let out = s.messages();
    let after_drop = out.split("已删除").last().unwrap_or("");
    assert!(!after_drop.contains("m_a"), "{out}");
}

#[test]
fn describe_without_argument_is_like_dt() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE m (a INT)");
    s.run_interactive("\\d");
    assert!(s.messages().contains("行数"), "{}", s.messages());
}

#[test]
fn describe_an_unknown_table_reports() {
    let mut s = buffered();
    s.run_interactive("\\d nope");
    assert!(s.messages().contains("没有表"), "{}", s.messages());
}

// ------------------------------------------------------------------ \o

#[test]
fn o_redirects_only_the_query_table() {
    let dir = TempDir::new("o");
    let out = dir.path().join("r.txt");

    let mut s = buffered();
    s.run_interactive("CREATE TABLE m (a INT)");
    s.run_interactive(&format!("\\o {}", out.display()));
    s.run_interactive("INSERT INTO m VALUES (7)");
    s.run_interactive("SELECT a FROM m");

    let written = std::fs::read_to_string(&out).unwrap();

    // 文件里只有结果表本身
    assert!(written.contains('a'), "{written}");
    assert!(written.contains('7'), "{written}");
    // 状态行与汇总**不该**进文件
    assert!(!written.contains('→'), "状态行不该进文件:{written}");
    assert!(!written.contains("成功"), "汇总不该进文件:{written}");
    assert!(!written.contains("建表成功"), "{written}");

    // 而它们确实在消息那一路
    let messages = s.messages();
    assert!(messages.contains("建表成功"), "{messages}");
    assert!(messages.contains("成功"), "{messages}");
}

#[test]
fn o_without_argument_returns_to_the_initial_sink() {
    let dir = TempDir::new("o2");
    let out = dir.path().join("r.txt");

    let mut s = buffered();
    s.run_interactive(&format!("\\o {}", out.display()));
    s.run_interactive("\\o");
    s.run_interactive("SELECT 1 + 1 AS n");

    assert!(s.query_output().contains('n'), "{:?}", s.query_output());
    assert!(s.query_output().contains('2'), "{:?}", s.query_output());
}

#[test]
fn o_failure_keeps_the_previous_sink() {
    let mut s = buffered();
    s.run_interactive("\\o /no/such/dir/r.txt");
    assert!(s.messages().contains("打不开"), "{}", s.messages());

    // 输出没丢:仍然回到原来的 buffer
    s.run_interactive("SELECT 1 + 1 AS n");
    assert!(s.query_output().contains('n'), "{:?}", s.query_output());
}

#[test]
fn query_table_never_leaks_into_messages() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE m (a INT)");
    s.run_interactive("INSERT INTO m VALUES (42)");

    // 只截 SELECT 这一段的消息看--INSERT 的状态行里回显了语句原文,本来就有 42
    let before = s.messages().len();
    s.run_interactive("SELECT a FROM m");
    let all = s.messages();
    let during_select = &all[before..];

    // 数据行只进查询那一路
    assert!(s.query_output().contains("42"), "{:?}", s.query_output());
    assert!(!during_select.contains("42"), "结果表漏进了消息:{during_select}");
    assert!(during_select.contains("1 行"), "{during_select}");
}

#[test]
fn errors_go_to_messages() {
    let mut s = buffered();
    s.run_interactive("SELECT * FROM nope");
    assert!(s.messages().contains("unknown symbol"), "{}", s.messages());
    assert!(s.query_output().is_empty(), "{:?}", s.query_output());
}

// ------------------------------------------------------------------ \i

#[test]
fn i_runs_a_script_file() {
    let dir = TempDir::new("i");
    let script = dir.path().join("s.sql");
    std::fs::write(&script, "CREATE TABLE m (a INT);\nINSERT INTO m VALUES (1);\nSELECT a FROM m;\n")
        .unwrap();

    let mut s = buffered();
    s.run_interactive(&format!("\\i {}", script.display()));

    let messages = s.messages();
    assert!(messages.contains("建表成功"), "{messages}");
    assert!(messages.contains("插入 1 行"), "{messages}");
    assert!(s.query_output().contains('1'), "{:?}", s.query_output());
}

#[test]
fn i_can_be_nested() {
    let dir = TempDir::new("nest");
    let inner = dir.path().join("inner.sql");
    let outer = dir.path().join("outer.sql");

    std::fs::write(&inner, "CREATE TABLE t (a INT);\nINSERT INTO t VALUES (5);\n").unwrap();
    std::fs::write(
        &outer,
        format!("\\echo 外层开始\n\\i {}\nSELECT a FROM t;\n", inner.display()),
    )
    .unwrap();

    let mut s = buffered();
    s.run_interactive(&format!("\\i {}", outer.display()));

    let messages = s.messages();
    assert!(messages.contains("外层开始"), "{messages}");
    assert!(messages.contains("插入 1 行"), "{messages}");
    assert!(s.query_output().contains('5'), "{:?}", s.query_output());
}

#[test]
fn i_missing_file_reports_and_the_session_keeps_working() {
    let mut s = buffered();
    s.run_interactive("\\i /no/such/file.sql");
    assert!(s.messages().contains("读不了"), "{}", s.messages());

    s.run_interactive("\\echo 还在");
    assert!(s.messages().contains("还在"));
}

#[test]
fn i_without_a_path_prints_usage() {
    let mut s = buffered();
    s.run_interactive("\\i");
    assert!(s.messages().contains("用法"), "{}", s.messages());
}

#[test]
fn i_recursion_is_capped_instead_of_looping_forever() {
    let dir = TempDir::new("rec");
    let looper = dir.path().join("loop.sql");
    // 自己包含自己
    std::fs::write(&looper, format!("\\i {}\n", looper.display())).unwrap();

    let mut s = buffered();
    s.run_interactive(&format!("\\i {}", looper.display()));

    assert!(s.messages().contains("嵌套超过"), "{}", s.messages());
}

// ------------------------------------------------------------------ \!

#[test]
fn shell_runs_interactively() {
    let mut s = buffered();
    s.run_interactive("\\! echo shell-alive");
    assert!(s.messages().contains("shell-alive"), "{}", s.messages());
}

#[test]
fn shell_is_refused_inside_scripts() {
    let dir = TempDir::new("shell");
    let script = dir.path().join("s.sql");
    std::fs::write(&script, "\\! echo should-not-run\n").unwrap();

    let mut s = buffered();
    s.run_interactive(&format!("\\i {}", script.display()));

    let messages = s.messages();
    assert!(messages.contains("不允许"), "{messages}");
    // 命令**没有**被执行
    assert!(!messages.contains("should-not-run"), "{messages}");
}

#[test]
fn shell_without_a_command_prints_usage() {
    let mut s = buffered();
    s.run_interactive("\\!");
    assert!(s.messages().contains("用法"), "{}", s.messages());
}

#[test]
fn shell_reports_a_nonzero_exit_code() {
    let mut s = buffered();
    s.run_interactive("\\! exit 3");
    assert!(s.messages().contains("退出码"), "{}", s.messages());
}

// -------------------------------------------------------------- 会话行为

#[test]
fn q_sets_the_quit_flag() {
    let mut s = buffered();
    assert!(!s.should_quit());
    s.run_interactive("\\q");
    assert!(s.should_quit());
}

#[test]
fn sql_and_metacommands_interleave_preserving_order() {
    let mut s = buffered();
    // 多行 SQL + 夹在中间的元命令
    s.run_interactive("CREATE TABLE m (\n  a INT\n);\n\\echo 中部\nINSERT INTO m VALUES (1);");

    let messages = s.messages();
    let created = messages.find("建表成功").expect("应当建表");
    let middle = messages.find("中部").expect("应当回显");
    let inserted = messages.find("插入 1 行").expect("应当插入");
    assert!(created < middle && middle < inserted, "顺序错了:{messages}");
}

#[test]
fn empty_input_does_nothing() {
    let mut s = buffered();
    s.run_interactive("");
    s.run_interactive("   \n  \n");
    assert_eq!(s.messages(), "");
    assert_eq!(s.query_output(), "");
}

#[test]
fn a_script_can_use_metacommands_themselves() {
    let dir = TempDir::new("meta");
    let script = dir.path().join("s.sql");
    let redirected = dir.path().join("r.txt");
    std::fs::write(
        &script,
        format!(
            "CREATE TABLE m (a INT);\nINSERT INTO m VALUES (9);\n\\o {}\nSELECT a FROM m;\n\\o\n\\dt\n",
            redirected.display()
        ),
    )
    .unwrap();

    let mut s = buffered();
    s.run_interactive(&format!("\\i {}", script.display()));

    // 脚本里的 \o 生效了:结果表进了文件
    let written = std::fs::read_to_string(&redirected).unwrap();
    assert!(written.contains('9'), "{written}");
    // \o 之后的 \dt 走到消息那一路
    assert!(s.messages().contains("行数"), "{}", s.messages());
}

// ------------------------------------------------------------ \timing

#[test]
fn timing_reports_every_statement() {
    let mut s = buffered();
    s.run_interactive("\\timing on");
    assert!(s.messages().contains("计时:开"), "{}", s.messages());
    assert!(s.timing());

    s.run_interactive("CREATE TABLE m (a INT);\nINSERT INTO m VALUES (1);");

    let out = s.messages();
    assert_eq!(out.matches("耗时: ").count(), 2, "两条语句两条计时:{out}");
    assert!(out.contains("成功 2 条,失败 0 条"), "{out}");
}

#[test]
fn timing_without_an_argument_toggles() {
    let mut s = buffered();
    assert!(!s.timing());

    s.run_interactive("\\timing");
    assert!(s.timing(), "无参是切换");
    s.run_interactive("\\timing");
    assert!(!s.timing());
}

#[test]
fn timing_off_goes_back_to_silent() {
    let mut s = buffered();
    s.run_interactive("\\timing on");
    s.run_interactive("SELECT 1 AS n");

    s.run_interactive("\\timing off");
    s.run_interactive("SELECT 2 AS n");

    assert_eq!(
        s.messages().matches("耗时").count(),
        1,
        "关掉之后不该再打:{}",
        s.messages()
    );
}

#[test]
fn timing_rejects_a_value_it_does_not_understand() {
    let mut s = buffered();
    s.run_interactive("\\timing yes");
    assert!(s.messages().contains("只认 on / off"), "{}", s.messages());
    assert!(!s.timing(), "认不出来的取值不该把它悄悄打开");
}

// ------------------------------------------- \set VERBOSE

#[test]
fn verbose_is_on_by_default_and_off_silences_the_status_lines() {
    let mut s = buffered();
    assert!(s.verbose(), "内存会话默认看得见状态行");

    s.run_interactive("\\set VERBOSE off");
    assert!(!s.verbose());
    assert_eq!(s.messages(), "", "关掉时连它自己的回显也不打");

    s.run_interactive("CREATE TABLE m (a INT);\nINSERT INTO m VALUES (1);\nSELECT a FROM m;");

    let out = s.messages();
    assert_eq!(out, "", "关掉之后不该再有状态行与汇总:{out}");
    // 结果表照旧走另一路
    assert!(s.query_output().contains('1'), "{}", s.query_output());

    // 变量名与取值都不分大小写;打开这一句要报一声(那时它已经开着)
    s.run_interactive("\\set verbose ON");
    assert!(s.verbose());
    assert!(s.messages().contains("VERBOSE:开"), "{}", s.messages());
    s.run_interactive("INSERT INTO m VALUES (2);");
    assert!(
        s.messages().contains("成功 1 条,失败 0 条"),
        "{}",
        s.messages()
    );
}

#[test]
fn verbose_off_does_not_silence_errors() {
    let mut s = buffered();
    s.run_interactive("\\set VERBOSE off");
    s.run_interactive("SELECT * FROM nope;");

    let out = s.messages();
    assert!(out.contains("unknown symbol"), "错误永远要打:{out}");
    assert_eq!(s.failed(), 1);
    assert!(!out.contains("成功"), "状态行与汇总还是不打:{out}");
}

#[test]
fn the_read_notice_and_the_redirect_notice_follow_verbose() {
    let dir = TempDir::new("verbose-include");
    let script = dir.path().join("s.sql");
    std::fs::write(&script, "SELECT 1 AS n;\n").unwrap();
    let redirected = dir.path().join("n.txt");
    let include = format!("\\i {}", script.display());

    // 开着:读入与去向都报
    let mut s = buffered();
    s.run_interactive(&include);
    assert!(s.messages().contains("读入"), "{}", s.messages());

    // 关掉:两条提示都不报,但结果表照样进 `\o` 的文件
    let mut s = buffered();
    s.run_interactive("\\set VERBOSE off");
    s.run_interactive(&include);
    let out = s.messages();
    assert!(!out.contains("读入"), "读入提示归 VERBOSE:{out}");
    assert!(!out.contains("成功"), "{out}");

    s.run_interactive(&format!("\\o {}", redirected.display()));
    assert!(!s.messages().contains("查询输出"), "{}", s.messages());
    s.run_interactive("SELECT 2 AS two;");
    let written = std::fs::read_to_string(&redirected).unwrap();
    assert!(written.contains('2'), "去向提示静了,结果还得写下去:{written}");
}

#[test]
fn set_verbose_rejects_a_value_it_does_not_understand() {
    let mut s = buffered();
    s.run_interactive("\\set VERBOSE yes");
    assert!(s.messages().contains("只认 on / off"), "{}", s.messages());
    assert!(s.verbose(), "认不出来的取值不该把开关拨到另一边");

    // 先关掉,再不给值:不给值就是打开(与 ON_ERROR_STOP 一致)
    s.run_interactive("\\set VERBOSE off");
    assert!(!s.verbose());
    s.run_interactive("\\set VERBOSE");
    assert!(s.verbose());
}

// ------------------------------------------- \set ON_ERROR_STOP

#[test]
fn on_error_stop_stops_at_the_first_error() {
    let mut s = buffered();
    s.run_interactive("\\set ON_ERROR_STOP on");
    assert!(s.on_error_stop());

    s.run_interactive(
        "CREATE TABLE m (a INT);\nINSERT INTO nope VALUES (1);\nINSERT INTO m VALUES (2);",
    );

    let out = s.messages();
    assert!(out.contains("建表成功"), "{out}");
    assert!(out.contains("unknown symbol"), "{out}");
    assert!(out.contains("已停止后续语句"), "{out}");
    assert!(!out.contains("插入 1 行到 m"), "第三条不该跑:{out}");
    assert_eq!(
        s.symbols().relation("m").unwrap().nrows(),
        0,
        "第三条确实没跑"
    );
}

#[test]
fn without_on_error_stop_the_script_keeps_going() {
    let mut s = buffered();
    s.run_interactive(
        "CREATE TABLE m (a INT);\nINSERT INTO nope VALUES (1);\nINSERT INTO m VALUES (2);",
    );

    let out = s.messages();
    assert!(out.contains("unknown symbol"), "{out}");
    assert!(!out.contains("已停止后续语句"), "{out}");
    assert_eq!(
        s.symbols().relation("m").unwrap().nrows(),
        1,
        "后面那条照样跑"
    );
}

#[test]
fn on_error_stop_stops_the_including_script_too() {
    let dir = TempDir::new("stop");
    let inner = dir.path().join("inner.sql");
    std::fs::write(&inner, "INSERT INTO nope VALUES (1);\n").unwrap();

    let mut s = buffered();
    s.run_interactive("\\set ON_ERROR_STOP on");
    s.run_interactive(&format!(
        "CREATE TABLE m (a INT);\n\\i {}\nINSERT INTO m VALUES (9);",
        inner.display()
    ));

    let out = s.messages();
    assert!(out.contains("unknown symbol"), "{out}");
    assert!(!out.contains("插入 1 行到 m"), "外层后面的语句也要停:{out}");
    assert_eq!(s.symbols().relation("m").unwrap().nrows(), 0);
}

#[test]
fn on_error_stop_stops_on_a_metacommand_error_too() {
    let mut s = buffered();
    s.run_interactive("\\set ON_ERROR_STOP on");
    s.run_interactive("\\i /no/such/file.sql\nSELECT 1 AS n");

    let out = s.messages();
    assert!(out.contains("读不了"), "{out}");
    assert!(out.contains("已停止后续语句"), "{out}");
    assert!(!out.contains("1 行"), "读文件失败之后那条 SELECT 不该跑:{out}");
}

#[test]
fn set_on_error_stop_off_resumes() {
    let mut s = buffered();
    s.run_interactive("\\set ON_ERROR_STOP on");
    s.run_interactive("\\set ON_ERROR_STOP off");
    assert!(!s.on_error_stop());

    s.run_interactive("SELECT 1 FROM nope;\nSELECT 2 AS n");
    let out = s.messages();
    assert!(!out.contains("已停止后续语句"), "{out}");
    assert!(out.contains("成功 1 条,失败 1 条"), "{out}");
}

#[test]
fn set_without_a_value_turns_it_on_like_psql() {
    let mut s = buffered();
    s.run_interactive("\\set ON_ERROR_STOP");
    assert!(s.on_error_stop());
}

#[test]
fn set_rejects_an_unknown_variable_and_a_bad_value() {
    let mut s = buffered();
    s.run_interactive("\\set FOO on");
    assert!(s.messages().contains("只认 ON_ERROR_STOP"), "{}", s.messages());

    let mut s = buffered();
    s.run_interactive("\\set ON_ERROR_STOP ture");
    assert!(s.messages().contains("只认 on / off"), "{}", s.messages());
    assert!(!s.on_error_stop(), "写错了就当没写,别悄悄打开");
}

#[test]
fn unknown_metacommand_lists_the_new_ones_too() {
    let mut s = buffered();
    s.run_interactive("\\bogus");
    let out = s.messages();
    assert!(out.contains("\\timing"), "{out}");
    assert!(out.contains("\\set"), "{out}");
}

// -------------------------------------------- 事务 / schema / search_path

#[test]
fn transaction_statements_report_one_status_line_each() {
    let mut s = buffered();
    s.run_interactive("BEGIN;");
    s.run_interactive("COMMIT;");

    let out = s.messages();
    assert!(out.contains("→ BEGIN"), "{out}");
    assert!(out.contains("→ COMMIT"), "{out}");
    // 没有结果集,所以查询那一路什么都不该有
    assert!(s.query_output().is_empty(), "{}", s.query_output());
}

#[test]
fn rollback_without_a_transaction_warns_and_keeps_going() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE p (a INT)");
    s.run_interactive("ROLLBACK;");
    s.run_interactive("INSERT INTO p VALUES (1)");

    let out = s.messages();
    assert!(out.contains("ROLLBACK"), "{out}");
    // 照 PostgreSQL 的口径:只给一句警告,**不是**错误(退出码也不该受影响)
    assert!(out.contains("警告"), "{out}");
    assert!(!out.contains("错误"), "{out}");
    assert_eq!(s.failed(), 0);
    // 默认不遇错就停:后面那条 INSERT 照样跑
    assert!(out.contains("插入 1 行"), "{out}");
}

#[test]
fn a_transaction_keeps_its_changes_in_memory_until_commit() {
    let mut s = buffered();
    s.run_interactive("CREATE TABLE p (a INT); INSERT INTO p VALUES (1)");

    // `BEGIN` 之后敲的两条都在事务里:会话自己的符号表**看得见**
    s.run_interactive("BEGIN;");
    s.run_interactive("INSERT INTO p VALUES (2)");
    s.run_interactive("SELECT a FROM p ORDER BY a");
    assert!(s.query_output().contains("1"), "{}", s.query_output());
    assert!(s.query_output().contains("2"), "{}", s.query_output());

    // `ROLLBACK` 之后只剩事务前那一行
    s.run_interactive("ROLLBACK;");
    s.run_interactive("SELECT a FROM p ORDER BY a");

    let out = s.messages();
    assert!(!out.contains("错误"), "{out}");

    // 查询输出是**累积**的:事务里那次 SELECT 打过一个 `2`,回滚后这次不该再有
    assert_eq!(
        s.query_output().matches('2').count(),
        1,
        "回滚后不该还能查到那一行:{}",
        s.query_output()
    );
}

#[test]
fn search_path_is_visible_in_dt_and_in_qualified_queries() {
    let mut s = buffered();
    s.run_interactive("CREATE SCHEMA study; SET search_path TO study;");
    s.run_interactive("CREATE TABLE movies (movieid INT)");
    s.run_interactive("INSERT INTO movies VALUES (7)");
    s.run_interactive("\\dt");
    s.run_interactive("SELECT movieid FROM study.movies");

    let out = s.messages();
    assert!(out.contains("search_path = study"), "{out}");
    // `\dt` 列的是**存下来的**名字(带前缀)
    assert!(out.contains("study.movies"), "{out}");
    // 限定名与非限定名查的是同一张表
    assert!(s.query_output().contains("7"), "{}", s.query_output());
}

#[test]
fn an_init_script_with_metacommands_and_schema_runs_twice() {
    let dir = TempDir::new("init");
    let path = dir.path().join("init.sql");
    std::fs::write(
        &path,
        "\\set ON_ERROR_STOP on\n\
         BEGIN;\n\
         DROP SCHEMA IF EXISTS study CASCADE;\n\
         CREATE SCHEMA study;\n\
         SET search_path TO study, public;\n\
         CREATE TABLE movies (movieid INT, title TEXT);\n\
         INSERT INTO movies VALUES (1, 'A');\n\
         COMMIT;\n\
         \\echo init done\n",
    )
    .unwrap();

    let mut s = buffered();
    for round in 1..=2 {
        s.run_interactive(&format!("\\i {}", path.display()));
        let out = s.messages();
        assert!(out.contains("init done"), "第 {round} 轮:{out}");
        assert!(!out.contains("错误"), "第 {round} 轮有错误:{out}");
    }
}
