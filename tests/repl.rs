//! 交互式终端里**不碰终端**的那部分:什么时候算"够一句",Tab 能补出什么.
//!
//! 真终端(raw mode,按键,渲染)交给 rustyline,端到端在 `tests/repl_pty.rs` 里
//! 用真 pty 跑;这里只测纯函数,所以又快又稳.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::cmd::complete;
use ydbms::cmd::repl::is_complete;
use ydbms::cmd::Catalog;
use ydbms::db::table::{Column, ColumnsShape, DataType};
use ydbms::db::PhysTab;
use ydbms::ir::{has_top_level_terminator, SymbolTable};

// ------------------------------------------------------ 够不够一句

#[test]
fn a_statement_needs_a_top_level_semicolon() {
    assert!(!is_complete("SELECT 1"));
    assert!(is_complete("SELECT 1;"));
    assert!(!is_complete("CREATE TABLE m (\n  a INT\n)"));
    assert!(is_complete("CREATE TABLE m (\n  a INT\n);"));
}

#[test]
fn semicolons_inside_strings_and_comments_do_not_count() {
    // 字符串里的分号不是语句结束--TODO 1.6 的验收点
    assert!(!is_complete("SELECT ';'"));
    assert!(is_complete("SELECT ';';"));
    assert!(!is_complete("SELECT 'it''s;'"));
    assert!(is_complete("SELECT 'it''s;';"));

    // 行注释
    assert!(!is_complete("SELECT 1 -- ;\n"));
    assert!(is_complete("SELECT 1 -- 注释\n;"));

    // 块注释(含嵌套)
    assert!(!is_complete("SELECT 1 /* ; */"));
    assert!(!is_complete("/* a /* ; */ b */"));
    assert!(has_top_level_terminator("/* a /* */ */ ;"));

    // 美元引用
    assert!(!is_complete("SELECT $$;$$"));
    assert!(is_complete("SELECT $$;$$;"));
    assert!(!is_complete("SELECT $tag$; $tag$"));
    assert!(has_top_level_terminator("SELECT $tag$; $tag$;"));
}

#[test]
fn a_metacommand_is_a_line_of_its_own() {
    assert!(is_complete("\\dt"));
    assert!(is_complete("\\i /no/such.sql"));

    // 元命令行在最后:挂着的 SQL 一起交出去(顺序由引擎的 split 保证)
    assert!(!is_complete("SELECT 1"));
    assert!(is_complete("SELECT 1\n\\echo hi"));
}

#[test]
fn explain_does_not_need_a_semicolon() {
    assert!(is_complete("EXPLAIN SELECT 1"));
    assert!(is_complete("explain select 1"));
    // 只是以 EXPLAIN 开头的词不算
    assert!(!is_complete("EXPLAINING"));
}

#[test]
fn blank_input_is_complete() {
    assert!(is_complete(""));
    assert!(is_complete("   \n\n"));
}

#[test]
fn a_terminator_scanner_survives_awkward_input() {
    // 不该 panic:没闭合的引号,孤零零的 `$`,多字节字符
    for text in ["'", "\"", "$", "$$", "/*", "中;", "SELECT '中文;"] {
        let _ = has_top_level_terminator(text);
        let _ = is_complete(text);
    }
    assert!(has_top_level_terminator("SELECT '中文';"));
}

// ------------------------------------------------------ Tab 能补什么

/// 一张 `t(a INT, b TEXT)` + 一张 `study.movies(a INT, b TEXT)` 的目录.
fn catalog() -> Catalog {
    let shape = ColumnsShape::new(vec![
        Column::new("a", DataType::Int),
        Column::new("b", DataType::Text),
    ]);

    let mut symbols = SymbolTable::new();
    symbols
        .define("t", PhysTab::new("t", &shape))
        .expect("建 t");
    symbols
        .define("study.movies", PhysTab::new("study.movies", &shape))
        .expect("建 study.movies");

    Catalog::from_symbols(&symbols)
}

/// 在行尾按 Tab 会写进去的那些文本.
fn inserts(line: &str, catalog: &Catalog) -> Vec<String> {
    complete::suggest(line, line.len(), catalog)
        .map(|found| found.items.into_iter().map(|item| item.insert).collect())
        .unwrap_or_default()
}

#[test]
fn keywords_complete_case_insensitively() {
    let c = catalog();
    assert_eq!(inserts("sel", &c), vec!["SELECT"]);
    assert_eq!(inserts("SEL", &c), vec!["SELECT"]);
    assert!(inserts("whe", &c).contains(&"WHERE".to_string()));
    assert!(inserts("gro", &c).contains(&"GROUP".to_string()));
}

#[test]
fn table_and_column_names_complete() {
    let c = catalog();
    assert!(inserts("FROM st", &c).contains(&"study.movies".to_string()));
    assert!(inserts("FROM ", &c).contains(&"t".to_string()));
    assert!(inserts("SELECT a", &c).contains(&"a".to_string()));
}

#[test]
fn a_qualified_name_completes_only_that_tables_columns() {
    let c = catalog();
    assert_eq!(inserts("SELECT t.", &c), vec!["t.a", "t.b"]);
    // 限定名用最后一段也能认出来
    assert_eq!(inserts("SELECT movies.", &c), vec!["movies.a", "movies.b"]);
    assert!(inserts("SELECT nosuch.", &c).is_empty());
}

#[test]
fn metacommand_names_complete() {
    let c = catalog();
    assert_eq!(inserts("\\ti", &c), vec!["\\timing"]);

    let all = inserts("\\", &c);
    for want in ["\\i", "\\set", "\\dt", "\\timing", "\\l", "\\du", "\\c", "\\connect"] {
        assert!(all.contains(&want.to_string()), "{want} 不在 {all:?}");
    }
}

#[test]
fn metacommand_arguments_complete() {
    let c = catalog();
    assert_eq!(inserts("\\set ON_", &c), vec!["ON_ERROR_STOP"]);
    assert_eq!(inserts("\\set VER", &c), vec!["VERBOSE"]);
    assert!(inserts("\\set ON_ERROR_STOP ", &c).contains(&"on".to_string()));
    assert!(inserts("\\set VERBOSE ", &c).contains(&"off".to_string()));
    assert!(inserts("\\timing ", &c).contains(&"off".to_string()));
    assert!(inserts("\\d ", &c).contains(&"t".to_string()));
    assert!(inserts("\\c ", &c).is_empty(), "手搓的目录里没有库名可补");

    // 没有候选的命令就是没有
    assert!(inserts("\\echo ", &c).is_empty());
}

#[test]
fn connect_completes_database_names() {
    let mut c = catalog();
    c.databases = vec!["filmdb".to_string(), "other".to_string()];

    assert_eq!(inserts("\\c ", &c), vec!["filmdb", "other"]);
    assert_eq!(inserts("\\c fi", &c), vec!["filmdb"]);
    assert_eq!(inserts("\\connect ot", &c), vec!["other"]);
    assert!(inserts("\\c nope", &c).is_empty());
}

#[test]
fn suggestions_replace_exactly_the_word_under_the_cursor() {
    let c = catalog();
    let line = "SELECT sel";
    let found = complete::suggest(line, line.len(), &c).expect("应当有候选");

    assert_eq!(found.start, 7, "从词的起点开始换");
    assert_eq!(found.end, line.len(), "换到光标为止");
    assert_eq!(found.items[0].insert, "SELECT");
}

#[test]
fn a_cursor_inside_a_multibyte_character_does_not_panic() {
    let c = catalog();
    // "表"是三个字节:光标落在字符中间也得夹到边界上,不能 panic
    let line = "SELECT 表";
    for cursor in 0..=line.len() {
        let _ = complete::suggest(line, cursor, &c);
    }
}

#[test]
fn nothing_to_complete_gives_no_candidates() {
    let c = catalog();
    assert!(
        complete::suggest("", 0, &c).is_none(),
        "空行按 Tab 不该刷一屏关键字"
    );
    assert!(complete::suggest("   ", 3, &c).is_none());
    assert!(complete::suggest("zzz", 3, &c).is_none());
}

// ------------------------------------------------------ 路径补全

#[test]
fn paths_complete_from_the_filesystem() {
    let dir = TempDir::new("complete");
    std::fs::write(dir.path().join("script.sql"), "SELECT 1;\n").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let c = catalog();
    let at = format!("{}/", dir.path().display());
    let found = inserts(&format!("\\i {at}"), &c);

    assert!(
        found.contains(&format!("{at}script.sql")),
        "文件要写成全路径:{found:?}"
    );
    assert!(
        found.contains(&format!("{at}sub/")),
        "目录要补一个 /:{found:?}"
    );

    // 前缀能过滤
    let found = inserts(&format!("\\i {at}sc"), &c);
    assert_eq!(found, vec![format!("{at}script.sql")]);
}

#[test]
fn a_path_that_cannot_be_read_gives_no_candidates() {
    let c = catalog();
    assert!(inserts("\\i /no/such/dir/", &c).is_empty());
}

// ------------------------------------------------------ 非 tty:管道里的脚本

/// 把一段输入从管道喂给真二进制,收它的输出.
///
/// 没有 tty 时走的是 `run_plain`--这正是 `ydbms < s.sql` 那一路.
fn run_binary(input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ydbms"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("起不了 ydbms");

    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(input.as_bytes()).expect("写输入");
    drop(stdin); // 关掉它 = EOF,脚本读完就结束

    let out = child.wait_with_output().expect("收输出");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_piped_script_stops_at_the_first_error_under_on_error_stop() {
    let out = run_binary("\\set ON_ERROR_STOP on\nSELECT 1 FROM nope;\nSELECT 2 AS second;\n");

    assert!(out.contains("unknown symbol"), "{out}");
    assert!(out.contains("已停止后续语句"), "{out}");
    assert!(
        !out.contains("second"),
        "ON_ERROR_STOP 之后不该再读后面的语句(psql 也是整个脚本停):{out}"
    );
}

#[test]
fn a_piped_script_without_the_switch_keeps_going() {
    let out = run_binary("SELECT 1 FROM nope;\nSELECT 2 AS second;\n");

    assert!(out.contains("unknown symbol"), "{out}");
    assert!(out.contains("second"), "没开开关就照旧继续跑:{out}");
}

#[test]
fn a_piped_script_accumulates_multiline_statements() {
    let out = run_binary("CREATE TABLE m (\n  a INT\n);\nINSERT INTO m VALUES (5);\nSELECT a FROM m;\n");

    assert!(out.contains("建表成功:m"), "{out}");
    assert!(out.contains("插入 1 行到 m"), "{out}");
    assert!(!out.contains("syntax error"), "{out}");
}

// ------------------------------------------------------ 临时目录

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 自清理的临时目录.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-repl-{}-{}-{}", std::process::id(), tag, n));
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
