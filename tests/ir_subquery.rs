//! TODO 0.2 的子查询:标量,`IN`,`EXISTS`,相关引用,派生表,`UNION`,`ANY`/`ALL`.
//!
//! 小合成表 + **手算的**期望值:
//!
//! ```text
//! students: 1 Ann(math)  2 Bob(math)  3 Cid(phys)  4 Dan(phys)
//! gpa:      (1,10,3) (1,11,4) (2,10,1) (2,11,2) (3,10,4)
//!           → 全校平均 14/5 = 2.8;Dan 一门课都没有
//! ```
//!
//! 结果都对照过 **PostgreSQL 18.6**(`tests/script/db1/query4.sql` 那六条也逐值比过).

use ydbms::db::table::{Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{compile, execute, run_script, SymbolTable};

const FIXTURE: &str = "
    CREATE TABLE students (sid INT, name TEXT, major TEXT);
    INSERT INTO students VALUES (1, 'Ann', 'math'), (2, 'Bob', 'math'),
                                (3, 'Cid', 'phys'), (4, 'Dan', 'phys');
    CREATE TABLE gpa (sid INT, course INT, score INT);
    INSERT INTO gpa VALUES (1, 10, 3), (1, 11, 4), (2, 10, 1), (2, 11, 2), (3, 10, 4);
";

fn db() -> SymbolTable {
    let mut symbols = SymbolTable::new();
    let results = run_script(FIXTURE, &mut symbols).expect("应当能解析");
    for result in &results {
        assert!(
            result.outcome.is_ok(),
            "第 {} 条失败了:{:?}",
            result.index,
            result.outcome
        );
    }
    symbols
}

fn q(symbols: &SymbolTable, sql: &str) -> PhysTab {
    let plan = compile(sql, symbols).unwrap_or_else(|e| panic!("`{sql}` 降级失败:{e}"));
    execute(&plan, symbols).unwrap_or_else(|e| panic!("`{sql}` 执行失败:{e}"))
}

fn names(symbols: &SymbolTable, sql: &str) -> Vec<String> {
    let out = q(symbols, sql);
    (0..out.nrows())
        .map(|r| out.value(r, 0).expect("没有值").to_string())
        .collect()
}

fn count(symbols: &SymbolTable, sql: &str) -> i32 {
    let out = q(symbols, sql);
    out.value(0, 0).and_then(|v| v.as_int()).expect("COUNT 是整数")
}

/// 期望被拒绝(降级或执行任一步都算).
fn rejected(symbols: &SymbolTable, sql: &str) -> String {
    match compile(sql, symbols) {
        Err(e) => e.to_string(),
        Ok(plan) => match execute(&plan, symbols) {
            Err(e) => e.to_string(),
            Ok(table) => panic!("`{sql}` 本应被拒绝,却返回了 {} 行", table.nrows()),
        },
    }
}

// ------------------------------------------------------------- 标量子查询

#[test]
fn an_uncorrelated_scalar_subquery_is_a_plain_value() {
    let symbols = db();
    // 全校平均 2.8:比它高的分数是 3,4,4 → 3 条
    assert_eq!(count(&symbols, "SELECT COUNT(*) FROM gpa WHERE score > (SELECT AVG(score) FROM gpa)"), 3);
}

#[test]
fn a_scalar_subquery_with_no_rows_is_null() {
    let symbols = db();
    let out = q(&symbols, "SELECT (SELECT score FROM gpa WHERE sid = 99) AS x");

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Null));
}

#[test]
fn a_scalar_subquery_with_more_than_one_row_is_an_error() {
    let symbols = db();
    // Ann 有两门课 → 标量子查询要求最多一行
    let err = rejected(&symbols, "SELECT (SELECT score FROM gpa WHERE sid = 1)");
    assert!(err.contains("标量子查询"), "{err}");
}

#[test]
fn a_scalar_subquery_must_have_exactly_one_column() {
    let symbols = db();
    let err = rejected(&symbols, "SELECT (SELECT sid, score FROM gpa)");
    assert!(err.contains("一列"), "{err}");
}

// ------------------------------------------------------------------ IN

#[test]
fn in_a_subquery_matches_the_members_of_its_result() {
    let symbols = db();
    // 分数 >= 3 的选课记录属于 sid 1 与 3
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE s.sid IN (SELECT g.sid FROM gpa AS g WHERE g.score >= 3)
             ORDER BY s.sid"
        ),
        vec!["Ann", "Cid"]
    );
}

#[test]
fn not_in_a_subquery_is_the_complement() {
    let symbols = db();
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE s.sid NOT IN (SELECT g.sid FROM gpa AS g WHERE g.score >= 3)
             ORDER BY s.sid"
        ),
        vec!["Bob", "Dan"]
    );
}

#[test]
fn not_in_with_a_null_in_the_inner_set_is_never_true() {
    let symbols = db();
    // 内层集合是 {NULL, 1, 2, 3}:`4 NOT IN (...)` 因为与 NULL 比较是 UNKNOWN 而**不为真**
    assert_eq!(
        count(
            &symbols,
            "SELECT COUNT(*) FROM students AS s
             WHERE s.sid NOT IN (
                 SELECT CASE WHEN g.score = 3 THEN NULL ELSE g.sid END FROM gpa AS g
             )"
        ),
        0
    );
}

#[test]
fn in_a_subquery_over_a_union_uses_both_branches() {
    let symbols = db();
    // sid 1(分数 4)或 sid 2(分数 1)都算命中
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE s.sid IN (
                 SELECT g.sid FROM gpa AS g WHERE g.score >= 4
                 UNION
                 SELECT g.sid FROM gpa AS g WHERE g.score <= 1
             )
             ORDER BY s.sid"
        ),
        vec!["Ann", "Bob", "Cid"]
    );
}

// -------------------------------------------------------------- EXISTS

#[test]
fn exists_is_about_having_rows_at_all() {
    let symbols = db();
    // 有 4 分记录的只有 Ann 与 Cid
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE EXISTS (SELECT 1 FROM gpa AS g WHERE g.sid = s.sid AND g.score >= 4)
             ORDER BY s.sid"
        ),
        vec!["Ann", "Cid"]
    );
}

#[test]
fn not_exists_is_the_complement() {
    let symbols = db();
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE NOT EXISTS (SELECT 1 FROM gpa AS g WHERE g.sid = s.sid AND g.score >= 4)
             ORDER BY s.sid"
        ),
        vec!["Bob", "Dan"]
    );
}

#[test]
fn exists_ignores_the_select_list() {
    let symbols = db();
    // `SELECT 1` / `SELECT *` 对 EXISTS 没有区别
    assert_eq!(
        count(
            &symbols,
            "SELECT COUNT(*) FROM students AS s WHERE EXISTS (SELECT * FROM gpa AS g WHERE g.sid = s.sid)"
        ),
        3
    );
}

// ------------------------------------------------------- 相关标量子查询

#[test]
fn a_correlated_scalar_subquery_is_recomputed_per_row() {
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT s.name, (SELECT MAX(g.score) FROM gpa AS g WHERE g.sid = s.sid) AS best
         FROM students AS s
         ORDER BY s.sid",
    );

    let got: Vec<(String, String)> = (0..out.nrows())
        .map(|r| (out.value(r, 0).expect("名字").to_string(), out.value(r, 1).expect("最高分").to_string()))
        .collect();

    // Dan 一门课都没有 → 内层 0 行 → NULL
    assert_eq!(
        got,
        vec![
            ("Ann".to_string(), "4".to_string()),
            ("Bob".to_string(), "2".to_string()),
            ("Cid".to_string(), "4".to_string()),
            ("Dan".to_string(), "NULL".to_string()),
        ]
    );
}

#[test]
fn a_correlated_scalar_subquery_can_be_averaged_and_rounded() {
    let symbols = db();
    // query4 [4-4] 的形状:`ROUND(AVG(...), 2)`
    assert_eq!(
        names(
            &symbols,
            "SELECT (SELECT ROUND(AVG(g.score), 2) FROM gpa AS g WHERE g.sid = s.sid) AS avg_score
             FROM students AS s
             WHERE s.sid = 1"
        ),
        vec!["3.5"]
    );
}

// ------------------------------------------------------- 派生表 / 集合运算

#[test]
fn a_derived_table_is_joined_like_a_relation() {
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT t.name, c.n
         FROM (SELECT sid, COUNT(*) AS n FROM gpa GROUP BY sid) AS c
             JOIN students AS t ON t.sid = c.sid
         ORDER BY t.sid",
    );

    let got: Vec<String> = (0..out.nrows())
        .map(|r| format!("{}={}", out.value(r, 0).expect("名字"), out.value(r, 1).expect("门数")))
        .collect();
    assert_eq!(got, vec!["Ann=2", "Bob=2", "Cid=1"]);
}

#[test]
fn a_derived_table_can_be_filtered_by_the_outer_query() {
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT t.sid, t.best FROM (SELECT sid, MAX(score) AS best FROM gpa GROUP BY sid) AS t
         WHERE t.best >= 4
         ORDER BY t.sid",
    );

    let got: Vec<String> = (0..out.nrows())
        .map(|r| format!("{}={}", out.value(r, 0).expect("sid"), out.value(r, 1).expect("最高分")))
        .collect();
    assert_eq!(got, vec!["1=4", "3=4"]);
}

#[test]
fn union_deduplicates_and_union_all_does_not() {
    let symbols = db();
    // gpa 的 sid 是 {1,2,3},students 的 sid 是 {1,2,3,4} → 并集 4 个,全并 9 个
    assert_eq!(count(&symbols, "SELECT COUNT(*) FROM (SELECT sid FROM gpa UNION SELECT sid FROM students) AS u"), 4);
    assert_eq!(
        count(&symbols, "SELECT COUNT(*) FROM (SELECT sid FROM gpa UNION ALL SELECT sid FROM students) AS u"),
        5 + 4
    );
}

#[test]
fn union_removes_the_rows_the_two_branches_share() {
    let symbols = db();
    // `score >= 3` 命中 3 行,`score >= 4` 命中 2 行;两支的 sid 都是 {1, 3}
    // → `UNION` 去重后 2 行,`UNION ALL` 原样 5 行
    let deduped = q(&symbols, "SELECT sid FROM gpa WHERE score >= 3 UNION SELECT sid FROM gpa WHERE score >= 4");
    let all = q(&symbols, "SELECT sid FROM gpa WHERE score >= 3 UNION ALL SELECT sid FROM gpa WHERE score >= 4");

    assert_eq!(deduped.nrows(), 2);
    assert_eq!(all.nrows(), 5);
}

// --------------------------------------------------------- ANY / ALL

#[test]
fn any_is_membership_and_all_is_every_member() {
    let symbols = db();
    // 只要有一门课的分数 >= 4
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE 4 <= ANY (SELECT g.score FROM gpa AS g WHERE g.sid = s.sid)
             ORDER BY s.sid"
        ),
        vec!["Ann", "Cid"]
    );
    // 每一门课的分数都 >= 2(Dan 没有课 → 空集 → ALL 为真)
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE 2 <= ALL (SELECT g.score FROM gpa AS g WHERE g.sid = s.sid)
             ORDER BY s.sid"
        ),
        vec!["Ann", "Cid", "Dan"]
    );
}

#[test]
fn all_over_an_empty_set_is_true_and_any_is_false() {
    let symbols = db();
    assert_eq!(
        count(&symbols, "SELECT COUNT(*) FROM students AS s WHERE 2 > ALL (SELECT score FROM gpa WHERE sid = 99)"),
        4
    );
    assert_eq!(
        count(&symbols, "SELECT COUNT(*) FROM students AS s WHERE 2 > ANY (SELECT score FROM gpa WHERE sid = 99)"),
        0
    );
}

#[test]
fn any_with_an_operator_is_supported() {
    let symbols = db();
    // `3 < ANY (...)`:Ann 的 4,Bob 的 无,Cid 的 4
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE 3 < ANY (SELECT g.score FROM gpa AS g WHERE g.sid = s.sid)
             ORDER BY s.sid"
        ),
        vec!["Ann", "Cid"]
    );
}

// ------------------------------------------------ 表达式层:CASE / OR / 联结

#[test]
fn exists_can_be_used_inside_case_and_or() {
    let symbols = db();
    // 子查询是**表达式**,不是只能当 WHERE 的整条谓词
    let out = q(
        &symbols,
        "SELECT s.name,
                CASE WHEN EXISTS (SELECT 1 FROM gpa AS g WHERE g.sid = s.sid) THEN 'yes' ELSE 'no' END AS has
         FROM students AS s
         ORDER BY s.sid",
    );
    let got: Vec<String> = (0..out.nrows())
        .map(|r| format!("{}={}", out.value(r, 0).expect("名字"), out.value(r, 1).expect("有课")))
        .collect();
    assert_eq!(got, vec!["Ann=yes", "Bob=yes", "Cid=yes", "Dan=no"]);

    // `OR` 两边各有一条路
    assert_eq!(
        names(
            &symbols,
            "SELECT s.name FROM students AS s
             WHERE s.sid = 2 OR EXISTS (SELECT 1 FROM gpa AS g WHERE g.sid = s.sid AND g.score >= 4)
             ORDER BY s.sid"
        ),
        vec!["Ann", "Bob", "Cid"]
    );
}

#[test]
fn a_correlated_subquery_survives_a_join_in_the_outer_query() {
    let symbols = db();
    // 外层是两表连接:WHERE 里的相关子查询面对的行是**整条连接的输出**,
    // 所以它不能被推到某一张表的 `Scan` 上去(推下去那一行就变了).
    let out = q(
        &symbols,
        "SELECT s.name FROM students AS s
             JOIN gpa AS g ON g.sid = s.sid
         WHERE EXISTS (SELECT 1 FROM gpa AS g2 WHERE g2.sid = g.sid AND g2.score > 3)
         ORDER BY s.name",
    );

    // 有 4 分记录的 sid 是 1(Ann 两条选课记录)与 3(Cid 一条)
    assert_eq!(out.nrows(), 3);
    let got = names(
        &symbols,
        "SELECT s.name FROM students AS s
             JOIN gpa AS g ON g.sid = s.sid
         WHERE EXISTS (SELECT 1 FROM gpa AS g2 WHERE g2.sid = g.sid AND g2.score > 3)
         ORDER BY s.name",
    );
    assert_eq!(got, vec!["Ann", "Ann", "Cid"]);
}

#[test]
fn two_levels_of_correlation_work() {
    let symbols = db();
    // 中间层引用外层(`g2.sid = g1.sid` 里的 g1 是中间层的行),最内层引用最外层(`s.sid`)
    let got = names(
        &symbols,
        "SELECT s.name FROM students AS s
         WHERE EXISTS (
             SELECT 1 FROM gpa AS g1
             WHERE g1.sid = s.sid
               AND EXISTS (SELECT 1 FROM gpa AS g2 WHERE g2.sid = g1.sid AND g2.score > s.sid)
         )
         ORDER BY s.sid",
    );
    // 手算:sid=1 → g1 有两行,g2 要求同 sid 且分数 > 1 → 有(3,4)→ 命中;
    // sid=2 → g2 要求分数 > 2 → 没有(1,2)→ 不命中;sid=3 → > 3 → 有(4)→ 命中;sid=4 → 无 g1 → 不命中
    assert_eq!(got, vec!["Ann", "Cid"]);
}

#[test]
fn a_derived_table_inside_a_subquery_can_still_reach_the_outer_query() {
    let symbols = db();
    // 派生表不能看同一层的兄弟项,但**可以**看更外层的查询(PostgreSQL 也允许)
    let got = names(
        &symbols,
        "SELECT s.name FROM students AS s
         WHERE EXISTS (
             SELECT 1 FROM (SELECT g.sid, MAX(g.score) AS best FROM gpa AS g GROUP BY g.sid) AS d
             WHERE d.sid = s.sid AND d.best >= 4
         )
         ORDER BY s.sid",
    );
    assert_eq!(got, vec!["Ann", "Cid"]);
}

// ------------------------------------------------------------- 该拒绝的

#[test]
fn a_scalar_subquery_under_limit_zero_is_never_evaluated() {
    let symbols = db();
    // 拉取式:`LIMIT 0` 一行都不拉,所以那个"会报错"的子查询(Ann 有两门课)
    // 根本不该被求值--惰性边界在行这一层是成立的.
    let out = q(&symbols, "SELECT (SELECT score FROM gpa WHERE sid = 1) FROM students LIMIT 0");
    assert_eq!(out.nrows(), 0);
}

#[test]
fn intersect_and_lateral_are_rejected() {
    let symbols = db();
    assert!(rejected(&symbols, "SELECT sid FROM gpa INTERSECT SELECT sid FROM students").contains("INTERSECT"));

    let err = rejected(
        &symbols,
        "SELECT s.name FROM students AS s, LATERAL (SELECT g.score FROM gpa AS g WHERE g.sid = s.sid) AS x",
    );
    assert!(err.contains("LATERAL"), "{err}");
}

#[test]
fn union_with_mismatched_column_counts_is_rejected() {
    let symbols = db();
    let err = rejected(&symbols, "SELECT sid FROM gpa UNION SELECT sid, score FROM gpa");
    assert!(err.contains("列数"), "{err}");
}

#[test]
fn explain_marks_the_correlated_subquery() {
    let symbols = db();
    let plan = compile(
        "SELECT s.name FROM students AS s WHERE EXISTS (SELECT 1 FROM gpa AS g WHERE g.sid = s.sid)",
        &symbols,
    )
    .expect("应当能降级");
    let rendered = plan.explain(&symbols).expect("应当能渲染");

    assert!(rendered.contains("<sublink:exists, correlated>"), "{rendered}");
}

#[test]
fn analyze_rejects_a_subquery_whose_outer_chain_is_wrong() {
    // 手工搭一份 IR:外层链记错了就**必须**在 analyze 处被抓住,而不是安静地
    // 读错行.这是子查询那条不变量(`outers[0] == 程序面对的那份 schema`)
    // 唯一的守卫.
    use ydbms::db::table::{Column, ColumnsShape, DataType, Table, TypeFamily};
    use ydbms::db::PhysTab;
    use ydbms::ir::expr::{SubMode, SubPlan};
    use ydbms::ir::{PlanBuilder, ProgBuilder, ScalarType, SymbolTable};

    // 内层:`SELECT 1`--单行零列的关系接一个常量投影
    let mut inner = PlanBuilder::new();
    let single = inner.single_row();
    let mut constants = ProgBuilder::new();
    let one = constants.int(1);
    let projected = inner
        .project(single, vec![constants.finish(one)], vec!["one".to_string()])
        .expect("内层投影");
    let inner_plan = inner.finish(projected).expect("内层计划");

    // 外层:`SELECT a FROM t WHERE <子查询>`
    let shape = ColumnsShape::new(vec![Column::new("a", DataType::Int)]);
    let mut tab = PhysTab::new("t", &shape);
    tab.insert(vec![Value::Int(1)]).expect("插入");
    let mut symbols = SymbolTable::new();
    symbols.define("t", tab).expect("注册 t");

    // 外层链里写一个**执行时不存在**的列:`a` 才是那一层的 schema
    let spec = SubPlan {
        plan: inner_plan,
        mode: SubMode::Exists,
        inner: ScalarType::Value(TypeFamily::Int),
        arity: 1,
        outers: vec![vec![Column::new("ghost", DataType::Int)]],
        correlated: true,
    };

    let mut predicate = ProgBuilder::new();
    let reg = predicate.subquery(spec);
    let predicate = predicate.finish(reg);

    let mut outer = PlanBuilder::new();
    let scan = outer.scan("t");
    let filtered = outer.filter(scan, predicate).expect("外层 Filter");
    let plan = outer.finish(filtered).expect("外层计划");

    let err = plan.analyze(&symbols).expect_err("外层链对不上,应当报非法");
    assert!(err.to_string().contains("外层"), "{err}");
}
