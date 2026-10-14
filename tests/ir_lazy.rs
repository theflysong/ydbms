//! TODO 2.2 的验收：`CASE` 的两个分支、`AND`/`OR` 的两侧**不再被提前求值**。
//!
//! 表达式程序（[`Prog`]）仍是扁平的寄存器机，但 `CASE` 的分支与 `AND`/`OR` 的
//! 右侧都成了**自足子程序**，只有被选中时才求值。于是死分支里的运行期错误
//! （除零之类）不再冒出来——`CASE` 的惰性是 SQL 标准保证的，`AND`/`OR` 则是
//! PostgreSQL 事实上会给的（标准只说求值顺序不保证）。
//!
//! 这里断言的全是**手算的期望值**：`CASE WHEN 1 = 0 THEN 1/0 ELSE 42 END` 在
//! PostgreSQL 18.6 上就是 42，`WHERE false AND (1/0 = 1)` 就是 0 行。

use ydbms::db::table::{DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, SymbolTable};

/// `x ∈ {0, 5, 20}`——`0` 用来证明"短路之后除零根本不会发生"。
const FIXTURE: &str = "
    CREATE TABLE t (x INT);
    INSERT INTO t VALUES (0), (5), (20);
";

fn db() -> SymbolTable {
    let mut db = SymbolTable::new();
    let results = run_script(FIXTURE, &mut db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了：{:?}", r.index, r.outcome);
    }
    db
}

/// 跑一条查询。
fn q(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows，得到 {other:?}"),
    }
}

/// 跑一条查询并要求失败。
fn failure(db: &mut SymbolTable, sql: &str) -> DbError {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome.expect_err("本应失败")
}

/// 一列整数。
fn ints(table: &PhysTab) -> Vec<i32> {
    (0..table.nrows())
        .map(|r| table.value(r, 0).and_then(|v| v.as_int()).expect("INT 列"))
        .collect()
}

// ---------------------------------------------------------------------- CASE

#[test]
fn a_false_condition_never_evaluates_the_then_branch() {
    let mut db = db();

    // 两条都对照过 PostgreSQL 18.6：死分支里的 1/0 不该冒出来
    let out = q(&mut db, "SELECT CASE WHEN 1 = 0 THEN 1/0 ELSE 42 END AS a");
    assert_eq!(out.value(0, 0), Some(Value::Int(42)));

    let out = q(&mut db, "SELECT CASE WHEN true THEN 1 ELSE 1/0 END AS a");
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
}

#[test]
fn a_live_branch_still_reports_its_errors() {
    let mut db = db();

    // 反向也要成立：选中的那一支里的除零**照旧**报错（不是把错误吞掉）
    let err = failure(&mut db, "SELECT CASE WHEN true THEN 1/0 ELSE 42 END AS a");
    assert!(err.to_string().contains("除以零"), "{err}");
}

#[test]
fn case_conditions_are_evaluated_in_order() {
    let mut db = db();

    // 第一个条件成立时，后面的条件根本不算：`WHEN true` 之后那个 `1/0` 不该报错
    let out = q(
        &mut db,
        "SELECT CASE WHEN true THEN 1 WHEN 1/0 = 1 THEN 2 ELSE 3 END AS a",
    );
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));

    // 反过来：第一个条件不成立、第二个条件里有除零 —— 那就该报错
    let err = failure(
        &mut db,
        "SELECT CASE WHEN false THEN 1 WHEN 1/0 = 1 THEN 2 ELSE 3 END AS a",
    );
    assert!(err.to_string().contains("除以零"), "{err}");
}

#[test]
fn case_per_row_skips_the_dead_branch() {
    let mut db = db();

    // 逐行短路：x = 0 那一行根本不会做除法
    let out = q(
        &mut db,
        "SELECT CASE WHEN x <> 0 THEN 100 / x ELSE -1 END AS a FROM t ORDER BY x",
    );
    assert_eq!(
        out.value(0, 0).and_then(|v| v.as_int()),
        Some(-1),
        "x = 0 走 ELSE"
    );
    assert_eq!(ints(&q(&mut db, "SELECT x FROM t WHERE x = 5")), vec![5]);
}

#[test]
fn a_dead_branch_may_contain_a_subquery() {
    let mut db = db();

    // 子查询也是自足程序：死分支里的它不会被求值
    let out = q(&mut db, "SELECT CASE WHEN true THEN 1 ELSE (SELECT 1/0) END AS a");
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));

    // 子查询活着时照常工作（相关子查询也走同一条路）
    let out = q(&mut db, "SELECT x FROM t WHERE x > (SELECT 0) ORDER BY x");
    assert_eq!(ints(&out), vec![5, 20]);
}

// ---------------------------------------------------------------- AND / OR

#[test]
fn a_false_left_hand_side_skips_the_right_one() {
    let mut db = db();

    // `WHERE false AND (1/0 = 1)`：0 行，且不报除零
    let out = q(&mut db, "SELECT x FROM t WHERE false AND (1/0 = 1)");
    assert_eq!(out.nrows(), 0);

    // 顺序就是书写顺序：左侧先算，所以这一条**会**报错（我们不做重排）
    let err = failure(&mut db, "SELECT x FROM t WHERE (1/0 = 1) AND false");
    assert!(err.to_string().contains("除以零"), "{err}");
}

#[test]
fn a_true_left_hand_side_skips_the_right_one_for_or() {
    let mut db = db();

    // `WHERE true OR (1/0 = 1)`：三行全留，不报除零
    let out = q(&mut db, "SELECT x FROM t WHERE true OR (1/0 = 1) ORDER BY x");
    assert_eq!(ints(&out), vec![0, 5, 20]);

    // FALSE 时右侧必须算：这里就该报错
    let err = failure(&mut db, "SELECT x FROM t WHERE false OR (1/0 = 1)");
    assert!(err.to_string().contains("除以零"), "{err}");
}

#[test]
fn unk_and_false_is_still_false() {
    let mut db = db();

    // 三值逻辑不能被短路改坏：NULL AND FALSE = FALSE（不是 UNKNOWN）
    let out = q(
        &mut db,
        "SELECT x FROM t WHERE (NULL = 1) AND false",
    );
    assert_eq!(out.nrows(), 0);

    // NULL OR TRUE = TRUE
    let out = q(&mut db, "SELECT x FROM t WHERE (NULL = 1) OR true ORDER BY x");
    assert_eq!(ints(&out), vec![0, 5, 20]);
}

#[test]
fn the_division_trap_row_is_filtered_before_the_division() {
    let mut db = db();

    // TODO 2.2 验收里那条：x 含 0，`x <> 0 AND 100/x > 10` 必须只留 x = 5, 20
    let out = q(&mut db, "SELECT x FROM t WHERE x <> 0 AND 1000/x > 10 ORDER BY x");
    assert_eq!(ints(&out), vec![5, 20]);
}

// ------------------------------------------------------ IN / BETWEEN 的短路

#[test]
fn in_and_between_short_circuit_too() {
    let mut db = db();

    // `IN` 折成 `=` 链、`NOT IN` 折成 `<>` 链；第一项就命中时后面那项不求值
    let out = q(&mut db, "SELECT x FROM t WHERE 0 IN (0, 1/0) ORDER BY x");
    assert_eq!(ints(&out), vec![0, 5, 20]);

    // 第一项不命中就得继续算——那时 `1/0` 该报错（顺序就是书写顺序）
    let err = failure(&mut db, "SELECT x FROM t WHERE 1 IN (0, 1/0)");
    assert!(err.to_string().contains("除以零"), "{err}");

    // `BETWEEN`：下界就不成立时不看上界，于是那个 1/0 不会被算
    let out = q(&mut db, "SELECT x FROM t WHERE 0 BETWEEN 1 AND 1/0");
    assert_eq!(out.nrows(), 0);

    // 下界成立时上界要算 —— 该报错就报错
    let err = failure(&mut db, "SELECT x FROM t WHERE x BETWEEN 0 AND 1/0");
    assert!(err.to_string().contains("除以零"), "{err}");
}

// ------------------------------------------------------------------ 类型放大

#[test]
fn case_widening_works_inside_a_grouped_query() {
    let mut db = db();

    // 分组查询里，程序面对的是 `Aggregate` 的**输出** schema（聚合结果列），
    // 类型探针必须用那一份——用错了会把合法的查询判成非法（标志对不上）
    let out = q(
        &mut db,
        "SELECT CASE WHEN COUNT(*) > 0 THEN COUNT(*) ELSE 1.5 END AS a FROM t",
    );
    assert_eq!(out.columns()[0].ty, DataType::NumericAny);
    assert_eq!(out.value(0, 0), Some(Value::Numeric("3".to_string())));

    // 分支引用**分组键**（也来自聚合输出）时同样要能探出类型
    let out = q(
        &mut db,
        "SELECT x, CASE WHEN x > 10 THEN x ELSE 1.5 END AS a FROM t GROUP BY x ORDER BY x",
    );
    assert_eq!(out.columns()[1].ty, DataType::NumericAny);
    assert_eq!(out.value(0, 1), Some(Value::Numeric("1.5".to_string())));
    assert_eq!(out.value(1, 1), Some(Value::Numeric("1.5".to_string())));
    assert_eq!(out.value(2, 1), Some(Value::Numeric("20".to_string())));
}

#[test]
fn case_widening_still_works_after_the_branches_became_lazy() {
    let mut db = db();

    // 两个分支都算完的年代靠"另一侧的类型"决定放大；现在靠 `Step::Case::widen`
    // （降级时按两个分支的类型算出来的标志）
    let out = q(&mut db, "SELECT CASE WHEN true THEN 1 ELSE 1.5 END AS a");
    assert_eq!(out.value(0, 0), Some(Value::Numeric("1".to_string())));

    let out = q(&mut db, "SELECT CASE WHEN true THEN x ELSE 1.5 END AS a FROM t ORDER BY a LIMIT 1");
    assert_eq!(out.value(0, 0), Some(Value::Numeric("0".to_string())));
}
