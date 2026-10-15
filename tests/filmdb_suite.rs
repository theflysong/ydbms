//! crate 内自带的电影数据库**小样本**的端到端 testbench.
//!
//! ```text
//! tests/script/filmdb/init.sql(建 study schema,设 search_path,`\i filmdb.sql`)
//!    → tests/script/filmdb/filmdb.sql(11 张表,68 行,自己包了 BEGIN; ... COMMIT;)
//!    → tests/script/filmdb/queries.sql(6 条查询,各 `\o ./queryN.txt`)
//! ```
//!
//! **它不读 crate 之外的任何东西.**考试那套 filmdb(3.8 MB / 7.6 万行 / 25 份
//! PostgreSQL 参照输出)留在仓库外由 `ydbms-test/` 跑;这里要的是**读取真实脚本文件
//! 的那条链路**--`\i` 的相对路径,`\o` 落到文件,进程工作目录的切换--外加照
//! PostgreSQL 18.6 手算出来的期望值(不再有 `expect/*.txt` 参照文件).
//!
//! # 期望值是手算的,而且**连行序一起核**
//!
//! 6 条查询都带 `ORDER BY`,所以不需要老那套"丢掉分隔线再排序"的归一化:表头与每一行
//! 都逐字对.`filmdb.sql` 的 DDL 刻意覆盖 `CHECK`(列级 + 表级)/`NOT NULL`/`DEFAULT`/
//! `CHAR(N)`/`VARCHAR(N)`/`TEXT`/`INT`/`DATE`/复合主键,以及主键,`UNIQUE`,外键
//! "接受但如实记为未强制"这条口径.
//!
//! # 进程级工作目录
//!
//! `init.sql` 与 `queries.sql` 里都是 **cwd 相对**的路径(`\i filmdb.sql`,
//! `\o ./queryN.txt`),所以装载段要把 cwd 切到 fixture 目录,查询段切到临时目录.
//! `set_current_dir` 是**进程级**的,所以同一个二进制里的测试要串行(不同测试文件
//! 是不同进程,天然互斥).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ydbms::cmd::{Session, Sink};
use ydbms::db::table::{Table, Value};
use ydbms::db::PhysTab;

/// 改工作目录的测试之间互斥(`set_current_dir` 是进程级的).
static CWD_LOCK: Mutex<()> = Mutex::new(());

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `tests/script/filmdb/` 的绝对路径.
fn fixture_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/script/filmdb");
    dir.canonicalize()
        .unwrap_or_else(|e| panic!("找不到 {}:{e}", dir.display()))
}

/// 一次"切目录 → 跑 → 切回来"的守卫.
///
/// `Drop` 里还原,断言 panic 了也不会把工作目录留在别处(后面的测试还要用).
struct Cwd {
    before: PathBuf,
}

impl Cwd {
    fn enter(dir: &Path) -> Self {
        let before = std::env::current_dir().expect("取当前目录");
        std::env::set_current_dir(dir).unwrap_or_else(|e| panic!("切到 {} 失败:{e}", dir.display()));
        Cwd { before }
    }
}

impl Drop for Cwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.before);
    }
}

/// 自清理的临时目录(`queries.sql` 的 `\o` 落在这里,不写进仓库).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-filmdb-{}-{}-{}", std::process::id(), tag, n));
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

/// 一个行数.
fn rows(session: &Session, table: &str) -> usize {
    session
        .symbols()
        .relation(table)
        .unwrap_or_else(|| panic!("没有表 `{table}`"))
        .nrows()
}

/// 装载整库:`init.sql`(内含 `\i filmdb.sql`),返回会话好让调用方继续查.
///
/// 装载段必须站在 fixture 目录里--`init.sql` 的 `\i filmdb.sql` 是 cwd 相对的.
fn loaded() -> Session {
    let fixture = fixture_dir();
    let mut session = Session::with_sinks(Sink::buffer(), Sink::buffer());

    let _cwd = Cwd::enter(&fixture);
    session.run_file(&fixture.join("init.sql"));

    let out = session.messages();
    assert!(!out.contains("错误"), "装载出错了:\n{out}");
    assert!(out.contains("study schema 初始化完成"), "{out}");
    assert!(!out.contains("失败 1 条") && !out.contains("失败 2 条"), "有语句失败:\n{out}");
    session
}

/// 在装载好的会话上直接跑一条查询(降级 + 执行).
fn query(session: &Session, sql: &str) -> PhysTab {
    let plan = ydbms::ir::compile(sql, session.symbols())
        .unwrap_or_else(|e| panic!("`{sql}` 降级失败:{e}"));
    ydbms::ir::execute(&plan, session.symbols())
        .unwrap_or_else(|e| panic!("`{sql}` 执行失败:{e}"))
}

/// 把一个渲染出来的结果表读成"表头 + 数据行".
///
/// 只丢掉全 `-` 的分隔线(表头与数据都逐字留着,-- 连行序一起核).
fn cells_of(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !line.trim().chars().all(|c| c == '-'))
        .map(|line| line.split('|').map(|cell| cell.trim().to_string()).collect())
        .collect()
}

/// 核一张结果表:表头 + 数据行,顺序也算.
fn assert_table(text: &str, expected: &[&[&str]], what: &str) {
    let got = cells_of(text);
    let want: Vec<Vec<String>> = expected
        .iter()
        .map(|row| row.iter().map(|cell| cell.to_string()).collect())
        .collect();
    assert_eq!(got, want, "{what} 与手算的期望值不一致(原文:\n{text})");
}

// ------------------------------------------------------------------ 装载

#[test]
fn the_fixture_loads_through_real_scripts() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let session = loaded();

    // 11 张表全在 `study.` 下(`search_path` 让非限定名落到那里)
    let mut in_schema: Vec<String> = session
        .symbols()
        .names()
        .into_iter()
        .filter(|name| name.starts_with("study."))
        .map(str::to_string)
        .collect();
    in_schema.sort();
    assert_eq!(
        in_schema,
        vec![
            "study.alt_titles",
            "study.countries",
            "study.credits",
            "study.films_francais",
            "study.forum_members",
            "study.forum_posts",
            "study.forum_topics",
            "study.merge_people",
            "study.movie_title_ft_index2",
            "study.movies",
            "study.people",
        ],
        "{in_schema:?}"
    );

    // 行数逐张对:这些数字来自 filmdb.sql 自己的 INSERT 条数(合计 68 行)
    for (table, expected) in [
        ("study.countries", 8),
        ("study.movies", 12),
        ("study.people", 8),
        ("study.credits", 15),
        ("study.forum_members", 4),
        ("study.forum_topics", 3),
        ("study.forum_posts", 5),
        ("study.films_francais", 4),
        ("study.alt_titles", 6),
        ("study.merge_people", 3),
        // 空表也要在
        ("study.movie_title_ft_index2", 0),
    ] {
        assert_eq!(rows(&session, table), expected, "{table}");
    }

    // 建表消息要如实说:CHECK 真强制,其余只记录
    let out = session.messages();
    assert!(out.contains("条 CHECK 已强制"), "{out}");
    assert!(out.contains("已接受但**未强制**"), "{out}");
}

#[test]
fn missing_columns_take_the_default_and_are_null_otherwise() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let session = loaded();

    // 第 8 个人不给 gender:补的是 DEFAULT '?',不是 NULL;first_name / died 没默认值,补 NULL
    let out = query(
        &session,
        "SELECT first_name, died, gender FROM study.people WHERE peopleid = 8",
    );
    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Null));
    assert_eq!(out.value(0, 1), Some(Value::Null));
    assert_eq!(out.value(0, 2), Some(Value::Text("?".to_string())));

    // 显式写出来的 NULL 与"缺列补 NULL"是同一件事
    let out = query(
        &session,
        "SELECT COUNT(*) FROM study.movies WHERE runtime IS NULL",
    );
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
}

// ------------------------------------------------------------------ 查询

/// 手算的期望结果:`(文件名, 表头 + 数据行)`.
///
/// 表头 + 每一行都逐字对,顺序一起核(6 条查询都带 `ORDER BY`);`NULL` 就是渲染出来
/// 的 `NULL` 三个字母.
type Expected = [&'static [&'static str]];

const EXPECTED: &[(&str, &Expected)] = &[
    // 1960 年以后的作品(按年份,再按编号)
    (
        "query0.txt",
        &[
            &["movieid", "title", "country_name"],
            &["6", "Lawrence of Arabia", "United Kingdom"],
            &["3", "Le Samourai", "France"],
            &["11", "2001: A Space Odyssey", "United Kingdom"],
            &["1", "Alien", "United States"],
            &["2", "Blade Runner", "United States"],
            &["8", "Amelie", "France"],
            &["9", "Spirited Away", "Japan"],
            &["5", "City of God", "Brazil"],
        ],
    ),
    // 拍片超过两部的洲:欧洲 6 部(fr 2 + gb 2 + it 2),北美 3 部(us 3);
    // 亚洲 2 部,南美 1 部都被 HAVING 丢掉
    (
        "query1.txt",
        &[
            &["continent", "films"],
            &["Europe", "6"],
            &["North America", "3"],
        ],
    ),
    // 片长为空(第 12 部)或超过 200 分钟(第 4 / 第 6 部)的作品;
    // 第 12 部**没有任何 credits 行**,所以左连接补出来的是 NULL
    (
        "query2.txt",
        &[
            &["movieid", "title", "credited_as"],
            &["4", "Seven Samurai", "D"],
            &["4", "Seven Samurai", "W"],
            &["6", "Lawrence of Arabia", "D"],
            &["12", "La Strada", "NULL"],
        ],
    ),
    // 有过作品的国家:8 个里 6 个有(德国,西班牙没有)
    (
        "query3.txt",
        &[
            &["country_code", "country_name"],
            &["br", "Brazil"],
            &["fr", "France"],
            &["gb", "United Kingdom"],
            &["it", "Italy"],
            &["jp", "Japan"],
            &["us", "United States"],
        ],
    ),
    // 片名里带 a/A 的 9 部;`LENGTH` 按字符数,`CASE` 按片长分档
    (
        "query4.txt",
        &[
            &["movieid", "title", "title_length", "runtime_class"],
            &["1", "Alien", "5", "standard"],
            &["2", "Blade Runner", "12", "standard"],
            &["3", "Le Samourai", "11", "standard"],
            &["4", "Seven Samurai", "13", "long"],
            &["6", "Lawrence of Arabia", "18", "long"],
            &["8", "Amelie", "6", "standard"],
            &["9", "Spirited Away", "13", "standard"],
            &["11", "2001: A Space Odyssey", "21", "standard"],
            &["12", "La Strada", "9", "unknown"],
        ],
    ),
    // 2024 年注册的成员(brian,cleo)+ 2025 年开的主题(second topic),
    // 按年份倒序:2025 那条排最前
    (
        "query5.txt",
        &[
            &["label", "joined_year"],
            &["second topic", "2025"],
            &["brian", "2024"],
            &["cleo", "2024"],
        ],
    ),
];

#[test]
fn the_query_script_writes_files_that_match_hand_computed_values() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = loaded();

    // `queries.sql` 自己 `\o ./queryN.txt`,所以把目录切到一棵临时树里跑,
    // 别把文件写进仓库
    let out_dir = TempDir::new("queries");
    let _cwd = Cwd::enter(out_dir.path());
    session.run_file(&fixture_dir().join("queries.sql"));

    let out = session.messages();
    assert!(!out.contains("错误"), "查询出错:\n{out}");

    for (file, expected) in EXPECTED {
        let text = std::fs::read_to_string(out_dir.path().join(file))
            .unwrap_or_else(|e| panic!("没有写出 {file}:{e}"));
        assert_table(&text, expected, file);
    }
}

// ------------------------------------------------ 子查询与聚合(直接查)

#[test]
fn subqueries_and_aggregates_agree_with_the_hand_computed_values() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let session = loaded();

    // 相关 `EXISTS`:写过 `W` 署名的人只有 Kurosawa(2)与 Melville(3)
    let out = query(
        &session,
        "SELECT p.peopleid FROM study.people AS p
         WHERE EXISTS (SELECT 1 FROM study.credits AS c
                       WHERE c.peopleid = p.peopleid AND c.credited_as = 'W')
         ORDER BY p.peopleid",
    );
    let ids: Vec<Value> = (0..out.nrows()).filter_map(|r| out.value(r, 0)).collect();
    assert_eq!(ids, vec![Value::Int(2), Value::Int(3)]);

    // 标量子查询:片长最长的作品
    let out = query(
        &session,
        "SELECT m.title FROM study.movies AS m
         WHERE m.runtime = (SELECT MAX(runtime) FROM study.movies)",
    );
    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Text("Lawrence of Arabia".to_string())));

    // `COUNT(DISTINCT ...)`:12 部作品来自 6 个国家
    let out = query(
        &session,
        "SELECT COUNT(DISTINCT country) AS countries FROM study.movies",
    );
    assert_eq!(out.value(0, 0), Some(Value::Int(6)));
}

// -------------------------------------------------- CHECK 与语句级原子

#[test]
fn a_check_violation_rolls_the_whole_statement_back() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut session = loaded();
    let before = rows(&session, "study.people");

    // 第二行卒年早于生年,过不了 `CHECK (died IS NULL OR died > born)`;
    // 第一行本来是好的,**整条语句**都不该留下痕迹(一条语句 = 一个隐式事务)
    session.run_interactive(
        "INSERT INTO study.people (peopleid, first_name, surname, born, died, gender) VALUES
             (9, 'Test', 'Nine', 1950, 1990, 'M'),
             (10, 'Test', 'Ten', 1950, 1900, 'M');",
    );

    let out = session.messages();
    assert!(out.contains("violates check constraint"), "{out}");
    assert!(out.contains("卒年不早于生年"), "错的是哪一条约束要说清:{out}");
    assert_eq!(
        rows(&session, "study.people"),
        before,
        "语句级原子:第一行也不许留下"
    );
}
