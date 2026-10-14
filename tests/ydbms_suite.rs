//! `ydbms-test/` 的端到端 testbench：**真实的建库、装载与查询脚本**。
//!
//! ```text
//! init.sql（建 study schema、设 search_path、\i filmdb.sql）
//!    → filmdb.sql（11 张表、76131 条 INSERT，约 7.6 万行）
//!    → correctness/queries.sql（25 条查询）
//! ```
//!
//! 这是 TODO 1.2 / 1.3 的验收：`checks` / `DEFAULT` / 表级约束 / `date` 都到位了，
//! `filmdb.sql` 才建得起来、装得进去。
//!
//! # 为什么默认不跑 `tests.sql`
//!
//! `tests.sql` 的 Test 1.2 是 `movies CROSS JOIN countries`（9204 × 185 ≈ 170 万行），
//! 而引擎会把**整个结果表**物化再渲染成一个字符串——那一步要几百 MB 内存。它属于
//! 手动验收（人跑一遍看有没有错），不进 `cargo test`。
//!
//! # 进程级工作目录
//!
//! `init.sql` 里写的是 `\i filmdb.sql`（相对路径），所以这里必须把进程的工作目录
//! 切到 `ydbms-test/`；`correctness/queries.sql` 又会 `\o ./queryN.txt` 往当前目录
//! 写文件。`set_current_dir` 是**进程级**的，所以同一个二进制里的测试要串行
//! （不同测试文件是不同进程，天然互斥）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ydbms::cmd::{Session, Sink};

/// 改工作目录的测试之间互斥（`set_current_dir` 是进程级的）。
static CWD_LOCK: Mutex<()> = Mutex::new(());

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `ydbms-test/` 的绝对路径。
fn suite_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ydbms-test");
    dir.canonicalize()
        .unwrap_or_else(|e| panic!("找不到 {}：{e}", dir.display()))
}

/// 一次「切目录 → 跑 → 切回来」的守卫。
///
/// `Drop` 里还原，断言 panic 了也不会把工作目录留在别处（后面的测试还要用）。
struct Cwd {
    before: PathBuf,
}

impl Cwd {
    fn enter(dir: &Path) -> Self {
        let before = std::env::current_dir().expect("取当前目录");
        std::env::set_current_dir(dir).unwrap_or_else(|e| panic!("切到 {} 失败：{e}", dir.display()));
        Cwd { before }
    }
}

impl Drop for Cwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.before);
    }
}

/// 自清理的临时目录（`correctness/queries.sql` 的 `\o` 落在这里）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-suite-{}-{}-{}", std::process::id(), tag, n));
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

/// 一个行数。
fn rows(session: &Session, table: &str) -> usize {
    session
        .symbols()
        .relation(table)
        .unwrap_or_else(|| panic!("没有表 `{table}`"))
        .nrows()
}

/// 装载整库：`init.sql`（内含 `\i filmdb.sql`），返回会话好让调用方继续查。
fn loaded() -> Session {
    let suite = suite_dir();
    let mut session = Session::with_sinks(Sink::buffer(), Sink::buffer());
    session.run_file(&suite.join("init.sql"));

    let out = session.messages();
    assert!(!out.contains("错误"), "装载出错了：\n{out}");
    assert!(out.contains("study schema 初始化完成"), "{out}");
    assert!(
        !out.contains("失败 1 条") && !out.contains("失败 2 条"),
        "有语句失败：\n{out}"
    );
    session
}

/// 整套跑一遍：装载 + 行数 + 25 条 correctness 查询。
///
/// **一次装载、两段断言**：装载本身约 4 秒，跑两遍不值得（测试之间又不能共享
/// `Session`——它不是 `Sync`，测试是并行跑的）。
/// 把一份"表格"文本归一化成**可比较的形状**：`(表头, 排好序的数据行)`。
///
/// 两边的渲染不一样——本引擎是 `a | b`、`NULL` 写成 `NULL`；psql 是对齐成定宽、
/// `NULL` 是**空单元格**，末尾还带一行 `(N rows)`。这里把那些差异抹平：
///
/// * 丢掉空行、分隔线（全是 `-`/`+`/空格）与 `(N rows)` 页脚；
/// * 每个单元格 `trim`，空单元格与 `NULL` 统一成 `NULL`；
/// * 数据行**排序后**比较——`queries.sql` 里一条 `ORDER BY` 都没有，行序本来就不保证。
fn normalize(text: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    lines.retain(|line| {
        let trimmed = line.trim();
        let is_footer = trimmed.starts_with('(')
            && trimmed.ends_with(')')
            && trimmed.contains("row");
        let is_separator = trimmed
            .chars()
            .all(|c| c == '-' || c == '+' || c == ' ');
        !is_footer && !is_separator
    });
    assert!(!lines.is_empty(), "空的结果表");

    let header = cells(lines[0]);
    let mut rows: Vec<Vec<String>> = lines[1..].iter().map(|line| cells(line)).collect();
    rows.sort();
    (header, rows)
}

/// 一行 → 一串单元格（`NULL` 与空单元格视为同一个东西）。
fn cells(line: &str) -> Vec<String> {
    line.split('|')
        .map(|cell| {
            let text = cell.trim();
            if text.is_empty() || text == "NULL" {
                "NULL".to_string()
            } else {
                text.to_string()
            }
        })
        .collect()
}

#[test]
fn the_ydbms_test_suite_runs() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let suite = suite_dir();
    let _cwd = Cwd::enter(&suite);

    let mut session = loaded();

    // 11 张表全在 `study.` 下（`search_path` 让非限定名落到那里）
    let names = session.symbols().names();
    let in_schema: Vec<&&str> = names.iter().filter(|n| n.starts_with("study.")).collect();
    assert_eq!(in_schema.len(), 11, "{names:?}");

    // 行数逐张对：这些数字来自 filmdb.sql 自己的 INSERT 条数
    for (table, expected) in [
        ("study.merge_people", 5),
        ("study.countries", 185),
        ("study.movies", 9204),
        ("study.people", 16489),
        ("study.credits", 46850),
        ("study.forum_members", 10),
        ("study.forum_topics", 3),
        ("study.forum_posts", 9),
        ("study.films_francais", 11),
        ("study.alt_titles", 3365),
        ("study.movie_title_ft_index2", 0),
    ] {
        assert_eq!(rows(&session, table), expected, "{table}");
    }

    // 建表消息要如实说：CHECK 真强制、其余只记录
    let out = session.messages();
    assert!(out.contains("条 CHECK 已强制"), "{out}");
    assert!(out.contains("已接受但**未强制**"), "{out}");

    // ---- correctness/queries.sql
    //
    // 它自己 `\o ./queryN.txt`，所以把目录切到一棵临时树里跑，别把文件写进仓库。
    let out_dir = TempDir::new("queries");
    let _cwd = Cwd::enter(out_dir.path());
    session.run_file(&suite.join("correctness/queries.sql"));

    let out = session.messages();
    assert!(!out.contains("错误"), "查询出错：\n{out}");
    assert!(out.contains("All queries are ran"), "{out}");

    // ---- 逐条与**仓库自带的参照结果**比对
    //
    // `correctness/expect/query*.txt` 是 PostgreSQL 18.6 上跑出来的 psql 输出。
    // 只断言"没报错"是不够的：25 条查询里任何一条**安静地少行/多行**都看不出来，
    // 而这次改的恰恰是 CASE 的惰性、类型放大、可空侧下推与索引访问路径。
    let expect_dir = suite.join("correctness/expect");
    let mut compared = 0usize;
    for entry in std::fs::read_dir(&expect_dir).expect("读 expect 目录") {
        let path = entry.expect("目录项").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("文件名")
            .to_string();
        if !name.starts_with("query") || !name.ends_with(".txt") {
            continue;
        }

        let ours = std::fs::read_to_string(out_dir.path().join(&name))
            .unwrap_or_else(|e| panic!("没有写出 {name}：{e}"));
        let expected = std::fs::read_to_string(&path).expect("读参照结果");
        assert_eq!(normalize(&ours), normalize(&expected), "{name} 与参照结果不一致");
        compared += 1;
    }
    assert_eq!(compared, 25, "correctness 应该有 25 条查询");
}
