//! 聚合,`GROUP BY`,`HAVING`,`CASE WHEN`,`ROUND`,NUMERIC 算术.
//!
//! 全部用**小合成表**,期望值是手算的--这样断言的是"算得对不对",而不是
//! "和我们自己的实现一致".

use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, SymbolTable};

/// 两张键 + 五个值,够覆盖每个聚合的分组行为.
///
/// ```text
/// g | v
/// a | 1, 2, 3      → COUNT 3, SUM 6,  AVG 2,   MIN 1,  MAX 3
/// b | 10, 20       → COUNT 2, SUM 30, AVG 15,  MIN 10, MAX 20
/// ```
const FIXTURE: &str = "
    CREATE TABLE t (g TEXT, v INT);
    INSERT INTO t VALUES ('a', 1), ('a', 2), ('a', 3), ('b', 10), ('b', 20);
";

fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

fn db(sql: &str) -> SymbolTable {
    let mut db = SymbolTable::new();
    ok(&mut db, sql);
    db
}

fn q(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows,得到 {other:?}"),
    }
}

fn failure(db: &mut SymbolTable, sql: &str) -> DbError {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome.expect_err("本应失败")
}

/// 取一列,拼成 `g=值` 的样子方便断言.
fn pairs(table: &PhysTab, key: usize, value: usize) -> Vec<String> {
    (0..table.nrows())
        .map(|r| {
            format!(
                "{}={}",
                table.value(r, key).expect("键"),
                table.value(r, value).expect("值")
            )
        })
        .collect()
}

/// 单个格子(无 GROUP BY 的整表聚合).
fn one(db: &mut SymbolTable, sql: &str) -> String {
    let table = q(db, sql);
    assert_eq!(table.nrows(), 1, "整表聚合应当只有一行");
    table.value(0, 0).expect("值").to_string()
}

// ------------------------------------------------------------ 五个聚合

#[test]
fn count_star_per_group() {
    let mut db = db(FIXTURE);
    let out = q(&mut db, "SELECT g, COUNT(*) AS n FROM t GROUP BY g ORDER BY g");
    assert_eq!(pairs(&out, 0, 1), vec!["a=3", "b=2"]);
}

#[test]
fn sum_min_max_per_group() {
    let mut db = db(FIXTURE);
    let out = q(
        &mut db,
        "SELECT g, SUM(v) AS s, MIN(v) AS lo, MAX(v) AS hi FROM t GROUP BY g ORDER BY g",
    );
    assert_eq!(pairs(&out, 0, 1), vec!["a=6", "b=30"]);
    assert_eq!(pairs(&out, 0, 2), vec!["a=1", "b=10"]);
    assert_eq!(pairs(&out, 0, 3), vec!["a=3", "b=20"]);
}

#[test]
fn avg_is_exact_numeric_division() {
    let mut db = db(FIXTURE);
    // a = 6/3 = 2;b = 30/2 = 15
    let out = q(&mut db, "SELECT g, AVG(v) AS m FROM t GROUP BY g ORDER BY g");
    assert_eq!(pairs(&out, 0, 1), vec!["a=2", "b=15"]);
}

#[test]
fn avg_returns_numeric_even_for_int_input() {
    // 与 PostgreSQL 一致:整数求平均返回 numeric
    let mut db = db("CREATE TABLE e (v INT); INSERT INTO e VALUES (1), (2)");
    let out = q(&mut db, "SELECT AVG(v) AS m FROM e");

    // 计算结果是不限定的 `NUMERIC`(与 PostgreSQL 的 `avg(numeric)` 一致)
    assert_eq!(out.columns()[0].ty, DataType::NumericAny);
    assert_eq!(out.value(0, 0), Some(Value::Numeric("1.5".to_string())));
}

#[test]
fn sum_of_int_stays_int() {
    let mut db = db(FIXTURE);
    let out = q(&mut db, "SELECT SUM(v) AS s FROM t");
    assert_eq!(out.columns()[0].ty, DataType::Int);
}

#[test]
fn count_skips_nulls_but_count_star_does_not() {
    let mut db = db("CREATE TABLE e (v INT); INSERT INTO e VALUES (1), (NULL), (3)");

    assert_eq!(one(&mut db, "SELECT COUNT(*) AS n FROM e"), "3");
    assert_eq!(one(&mut db, "SELECT COUNT(v) AS n FROM e"), "2");
}

#[test]
fn count_distinct() {
    let mut db = db("CREATE TABLE e (v INT); INSERT INTO e VALUES (1), (1), (2), (3), (3), (3)");

    assert_eq!(one(&mut db, "SELECT COUNT(*) AS n FROM e"), "6");
    assert_eq!(one(&mut db, "SELECT COUNT(DISTINCT v) AS n FROM e"), "3");
}

#[test]
fn min_max_work_on_text_and_timestamp() {
    let mut db = db(
        "CREATE TABLE e (s TEXT, ts TIMESTAMP);
         INSERT INTO e VALUES ('b', '2024-06-01'), ('a', '1999-12-31'), ('c', '2024-01-15')",
    );

    assert_eq!(one(&mut db, "SELECT MIN(s) AS x FROM e"), "a");
    assert_eq!(one(&mut db, "SELECT MAX(s) AS x FROM e"), "c");
    assert_eq!(
        one(&mut db, "SELECT MIN(ts) AS x FROM e"),
        "1999-12-31 00:00:00.000000"
    );
}

// ------------------------------------------------------------ 分组形状

#[test]
fn group_by_two_keys() {
    let mut db = db(
        "CREATE TABLE e (a TEXT, b TEXT, v INT);
         INSERT INTO e VALUES ('x','p',1), ('x','q',2), ('x','p',4), ('y','p',8)",
    );
    let out = q(
        &mut db,
        "SELECT a, b, SUM(v) AS s FROM e GROUP BY a, b ORDER BY a, b",
    );

    assert_eq!(pairs(&out, 0, 2), vec!["x=5", "x=2", "y=8"]);
}

#[test]
fn without_group_by_the_whole_table_is_one_group() {
    let mut db = db(FIXTURE);
    let out = q(&mut db, "SELECT COUNT(*) AS n, SUM(v) AS s, MIN(v) AS lo FROM t");

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Int(5)));
    assert_eq!(out.value(0, 1), Some(Value::Int(36)));
    assert_eq!(out.value(0, 2), Some(Value::Int(1)));
}

#[test]
fn aggregating_an_empty_table_still_yields_one_row() {
    let mut db = db("CREATE TABLE e (v INT)");
    let out = q(&mut db, "SELECT COUNT(*) AS n, SUM(v) AS s, AVG(v) AS m, MIN(v) AS lo FROM e");

    assert_eq!(out.nrows(), 1, "没有 GROUP BY 时,空输入也要出一行");
    assert_eq!(out.value(0, 0), Some(Value::Int(0)), "COUNT(*) 是 0");
    assert_eq!(out.value(0, 1), Some(Value::Null), "SUM 是 NULL");
    assert_eq!(out.value(0, 2), Some(Value::Null), "AVG 是 NULL");
    assert_eq!(out.value(0, 3), Some(Value::Null), "MIN 是 NULL");
}

#[test]
fn grouping_an_empty_table_yields_no_rows() {
    let mut db = db("CREATE TABLE e (v INT)");
    let out = q(&mut db, "SELECT v, COUNT(*) AS n FROM e GROUP BY v");

    assert_eq!(out.nrows(), 0);
}

#[test]
fn nulls_form_their_own_group_in_first_appearance_order() {
    let mut db = db("CREATE TABLE e (v INT); INSERT INTO e VALUES (1), (NULL), (1)");
    let out = q(&mut db, "SELECT v, COUNT(*) AS n FROM e GROUP BY v");

    // 两行 NULL 会被归成**一组**(NULL 不等于 NULL,但分组把它们算一起),
    // 顺序是分组键首次出现的顺序:先 1 后 NULL
    assert_eq!(out.nrows(), 2);
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
    assert_eq!(out.value(0, 1), Some(Value::Int(2)));
    assert_eq!(out.value(1, 0), Some(Value::Null));
    assert_eq!(out.value(1, 1), Some(Value::Int(1)));
}

// ---------------------------------------------------------------- HAVING

#[test]
fn having_filters_groups() {
    let mut db = db(FIXTURE);
    let out = q(
        &mut db,
        "SELECT g, SUM(v) AS s FROM t GROUP BY g HAVING SUM(v) > 10 ORDER BY g",
    );

    assert_eq!(out.nrows(), 1);
    assert_eq!(pairs(&out, 0, 1), vec!["b=30"]);
}

#[test]
fn having_can_introduce_an_aggregate_not_in_the_select_list() {
    // HAVING 里第一次出现的聚合也要被收进 Aggregate 节点--这就是为什么
    // SELECT 列表与 HAVING 都必须在建节点**之前**降级完
    let mut db = db(FIXTURE);
    let out = q(
        &mut db,
        "SELECT g FROM t GROUP BY g HAVING MAX(v) > 5 ORDER BY g",
    );

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Text("b".to_string())));
}

#[test]
fn having_on_a_computed_ratio() {
    let mut db = db(FIXTURE);
    // a: 3/3 全 > 0 的只有 0 个 → 0%;b: 2/2 → 100%
    let out = q(
        &mut db,
        "SELECT g FROM t GROUP BY g \
         HAVING 100.0 * SUM(CASE WHEN v > 5 THEN 1 ELSE 0 END) / COUNT(*) >= 50.0 ORDER BY g",
    );

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Text("b".to_string())));
}

// ------------------------------------------------------------- ORDER BY

#[test]
fn order_by_an_aggregate_alias() {
    let mut db = db(FIXTURE);
    let out = q(
        &mut db,
        "SELECT g, SUM(v) AS s FROM t GROUP BY g ORDER BY s DESC",
    );
    assert_eq!(pairs(&out, 0, 1), vec!["b=30", "a=6"]);
}

#[test]
fn order_by_an_aggregate_that_is_not_selected() {
    let mut db = db(FIXTURE);
    let out = q(&mut db, "SELECT g FROM t GROUP BY g ORDER BY SUM(v) DESC");
    // b 的和是 30 > a 的 6
    assert_eq!(out.value(0, 0), Some(Value::Text("b".to_string())));
}

// ------------------------------------------------------------ CASE / ROUND

#[test]
fn case_when_classifies_rows() {
    let mut db = db(FIXTURE);
    let out = q(
        &mut db,
        "SELECT g, SUM(CASE WHEN v >= 2 THEN 1 ELSE 0 END) AS big FROM t GROUP BY g ORDER BY g",
    );

    // a: 2,3 两个 >= 2 → 2;b: 10,20 两个 → 2
    assert_eq!(pairs(&out, 0, 1), vec!["a=2", "b=2"]);
}

#[test]
fn case_without_else_yields_null() {
    let mut db = db("CREATE TABLE e (v INT); INSERT INTO e VALUES (1)");
    let out = q(&mut db, "SELECT CASE WHEN v > 5 THEN 'big' END AS label FROM e");

    assert_eq!(out.value(0, 0), Some(Value::Null));
}

#[test]
fn round_is_half_away_from_zero() {
    let mut db = SymbolTable::new();

    assert_eq!(one(&mut db, "SELECT ROUND(2.5, 0) AS x"), "3");
    assert_eq!(one(&mut db, "SELECT ROUND(-2.5, 0) AS x"), "-3");
    assert_eq!(one(&mut db, "SELECT ROUND(3.145, 2) AS x"), "3.15");
    assert_eq!(one(&mut db, "SELECT ROUND(3.144, 2) AS x"), "3.14");
}

#[test]
fn round_with_one_argument_goes_to_whole_numbers() {
    let mut db = SymbolTable::new();
    assert_eq!(one(&mut db, "SELECT ROUND(2.6) AS x"), "3");
    assert_eq!(one(&mut db, "SELECT ROUND(2.4) AS x"), "2");
}

#[test]
fn round_of_null_is_null() {
    let mut db = SymbolTable::new();
    let out = q(&mut db, "SELECT ROUND(NULL, 2) AS x");
    assert_eq!(out.value(0, 0), Some(Value::Null));
}

// --------------------------------------------------------- NUMERIC 算术

#[test]
fn numeric_add_sub_mul() {
    let mut db = SymbolTable::new();

    assert_eq!(one(&mut db, "SELECT 1.5 + 2.25 AS x"), "3.75");
    assert_eq!(one(&mut db, "SELECT 1.5 - 2.25 AS x"), "-0.75");
    assert_eq!(one(&mut db, "SELECT 1.5 * 2.5 AS x"), "3.75");
}

#[test]
fn division_scale_matches_postgresql() {
    // 每个期望值都是在临时起的 **PostgreSQL 18.6** 上实测出来的,不是照文档推的.
    //
    // 标度由 `select_div_scale` 决定,其中"领头数字组更小就把 qweight 再减一"
    // 那一步最容易漏--漏了会给少 4 位的结果,而 `=` 的真假会因此**反过来**.
    let mut db = SymbolTable::new();
    for (expr, want) in [
        // 领头组 1 < 3 → 修正 → 20 位
        ("1.0 / 3", "0.33333333333333333333"),
        ("2.0 / 3", "0.66666666666666666667"),
        ("1.0 / 7", "0.14285714285714285714"),
        ("1.00 / 3", "0.33333333333333333333"),
        // 领头组 100 >= 28 → 不修正 → 16 位
        ("100.0 / 28", "3.5714285714285714"),
        ("2500.0 / 28", "89.2857142857142857"),
        // 领头组 1 < 7 → 修正,把 qweight 从 1 拉回 0 → 16 位(漏掉会给 12 位)
        ("12345.0 / 7", "1763.5714285714285714"),
        ("1.0 / 300", "0.00333333333333333333"),
        ("1.0 / 0.3", "3.3333333333333333"),
    ] {
        assert_eq!(one(&mut db, &format!("SELECT {expr} AS x")), want, "{expr}");
    }
}

#[test]
fn int_times_numeric_promotes_to_numeric() {
    // 对应 PostgreSQL 的 int → numeric 隐式转换
    let mut db = SymbolTable::new();

    assert_eq!(one(&mut db, "SELECT 100 * 1.5 AS x"), "150");
    let out = q(&mut db, "SELECT 2 * 3.0 AS x");
    assert_eq!(out.columns()[0].ty, DataType::NumericAny);
}

#[test]
fn int_arithmetic_stays_int() {
    let mut db = SymbolTable::new();
    let out = q(&mut db, "SELECT 7 / 2 AS x");

    assert_eq!(out.columns()[0].ty, DataType::Int, "两个 INT 相除仍是整除");
    assert_eq!(out.value(0, 0), Some(Value::Int(3)));
}

#[test]
fn an_extreme_scale_division_is_correct_rather_than_zero() {
    // 历史:这段曾经返回**安静的 0**--`pow10` 溢出返回 0,`checked_mul(0)` 是
    // `Some(0)`,于是溢出被吞掉,除法的分子变成 0.标度 34 把 `10^40` 推过了 i128.
    // 换成 bigdecimal 之后这条溢出路径没有了,但断言要留着钉住"不再是 0".
    let mut db = db(
        "CREATE TABLE t (a NUMERIC(38,34), b NUMERIC(38,34));
         INSERT INTO t VALUES (0.1234567890123456789012345678901234,
                               0.9876543210987654321098765432109876)",
    );

    let out = q(&mut db, "SELECT a / b AS x FROM t");
    let text = out.value(0, 0).expect("值").to_string();
    let value: f64 = text.parse().unwrap_or_else(|_| panic!("不是数:{text}"));

    assert!(
        (0.124..0.126).contains(&value),
        "应当约等于 0.125,实际 {text}"
    );
}

#[test]
fn division_by_zero_is_an_error() {
    let mut db = SymbolTable::new();
    let err = failure(&mut db, "SELECT 1.5 / 0 AS x");
    assert!(err.to_string().contains("零"), "{err}");
}

#[test]
fn the_query3_pass_rate_shape_end_to_end() {
    // 这就是 query3 [3-4] / [3-6] 里那个表达式
    let mut db = db(FIXTURE);
    let out = q(
        &mut db,
        "SELECT g, ROUND(100.0 * SUM(CASE WHEN v > 5 THEN 1 ELSE 0 END) / COUNT(*), 1) AS rate \
         FROM t GROUP BY g ORDER BY g",
    );

    assert_eq!(pairs(&out, 0, 1), vec!["a=0", "b=100"]);
}

#[test]
fn rounded_division_matches_hand_computation() {
    let mut db = SymbolTable::new();
    // 100.0 * 25 / 28 = 89.285714... → 89.3
    assert_eq!(one(&mut db, "SELECT ROUND(100.0 * 25 / 28, 1) AS x"), "89.3");
}

// ------------------------------------------------- 放开精度之后的新能力

#[test]
fn decimal_arithmetic_is_exact_not_binary_floating_point() {
    let mut db = SymbolTable::new();
    // 浮点会给出 0.30000000000000004
    assert_eq!(one(&mut db, "SELECT 0.1 + 0.2 AS x"), "0.3");
    assert_eq!(one(&mut db, "SELECT 0.3 - 0.1 AS x"), "0.2");
}

#[test]
fn round_supports_negative_digits() {
    let mut db = SymbolTable::new();

    assert_eq!(one(&mut db, "SELECT ROUND(1234.5, -2) AS x"), "1200");
    assert_eq!(one(&mut db, "SELECT ROUND(15, -1) AS x"), "20");
    assert_eq!(one(&mut db, "SELECT ROUND(1234.5, -5) AS x"), "0");
}

#[test]
fn a_bare_numeric_holds_far_more_than_38_digits() {
    // 这一条是"放开精度"的核心:过去 i128 只装得下 38 位有效数字
    let mut db = db("CREATE TABLE t (v NUMERIC)");
    let digits = "123456789012345678901234567890123456789012345678901234567890.5";
    ok(&mut db, &format!("INSERT INTO t VALUES ({digits})"));

    // NUMERIC(38,18) 会拒绝它,裸 NUMERIC 不会
    let out = q(&mut db, "SELECT v FROM t");
    assert_eq!(out.value(0, 0), Some(Value::Numeric(digits.to_string())));
}

#[test]
fn a_constrained_column_still_cannot_hold_more_than_its_precision() {
    // 放开的是"默认",不是"约束":写了 NUMERIC(N,M) 还是要守
    let mut db = db("CREATE TABLE t (v NUMERIC(6,2))");
    let err = failure(
        &mut db,
        "INSERT INTO t VALUES (12345.678)",
    );
    assert!(err.to_string().contains("NUMERIC(6,2)"), "{err}");
}

#[test]
fn numeric_with_a_single_typmod_means_scale_zero() {
    // 与 PostgreSQL 一致:`NUMERIC(N)` 等于 `NUMERIC(N, 0)`
    let mut db = db("CREATE TABLE t (v NUMERIC(5))");
    ok(&mut db, "INSERT INTO t VALUES (12345)");

    let err = failure(&mut db, "INSERT INTO t VALUES (1.5)");
    assert!(err.to_string().contains("小数位"), "{err}");
}

#[test]
fn big_numbers_survive_multiplication() {
    let mut db = SymbolTable::new();
    let out = q(
        &mut db,
        "SELECT 123456789012345678901234567890.5 * 1000000000000000000000000000000.5 AS x",
    );
    // 60 位精度的乘法结果,i128 时代根本做不了
    let text = out.value(0, 0).expect("值").to_string();
    assert!(text.len() > 50, "结果应当很长,实际 {text}");
    assert!(text.starts_with("123456789012345678901234567890"), "{text}");
}

// ------------------------------------------------------------ 拒绝的写法

#[test]
fn selecting_a_non_grouped_column_is_an_error() {
    let mut db = db(FIXTURE);
    let err = failure(&mut db, "SELECT g, v FROM t GROUP BY g");

    assert!(err.to_string().contains("GROUP BY"), "{err}");
}

#[test]
fn an_aggregate_in_where_is_an_error() {
    let mut db = db(FIXTURE);
    let err = failure(&mut db, "SELECT g FROM t WHERE COUNT(*) > 1");
    assert!(matches!(err, DbError::Unsupported { .. }), "{err}");
}

#[test]
fn window_functions_are_rejected() {
    let mut db = db(FIXTURE);
    let err = failure(&mut db, "SELECT g, SUM(v) OVER (PARTITION BY g) AS s FROM t");
    assert!(err.to_string().contains("窗口"), "{err}");
}

#[test]
fn an_aggregate_filter_clause_keeps_only_the_matching_rows() {
    let mut db = db(FIXTURE);
    // 手算:`v > 1` 命中 2,3,10,20 → COUNT 4 / SUM 35;
    // 而 `COUNT(*)`(没有 FILTER)是 5 行
    let out = q(
        &mut db,
        "SELECT COUNT(*) FILTER (WHERE v > 1) AS n, SUM(v) FILTER (WHERE v > 1) AS s,
                COUNT(*) AS all_rows
         FROM t",
    );

    assert_eq!(out.value(0, 0), Some(Value::Int(4)));
    assert_eq!(out.value(0, 1), Some(Value::Int(35)));
    assert_eq!(out.value(0, 2), Some(Value::Int(5)));
}

#[test]
fn an_aggregate_filter_with_a_group_by_filters_per_group() {
    let mut db = db(FIXTURE);
    // a 组:1,2,3 里 > 1 的是 2,3 → 2 行;b 组:10,20 全中 → 2 行
    let out = q(
        &mut db,
        "SELECT g, COUNT(*) FILTER (WHERE v > 1) AS n
         FROM t
         GROUP BY g
         ORDER BY g",
    );

    assert_eq!(out.value(0, 0), Some(Value::Text("a".to_string())));
    assert_eq!(out.value(0, 1), Some(Value::Int(2)));
    assert_eq!(out.value(1, 1), Some(Value::Int(2)));
}

#[test]
fn grouping_sets_are_rejected() {
    let mut db = db(FIXTURE);
    let err = failure(&mut db, "SELECT g FROM t GROUP BY ROLLUP (g)");
    assert!(matches!(err, DbError::Unsupported { .. }), "{err}");
}

#[test]
fn distinct_on_is_still_rejected() {
    let mut db = db(FIXTURE);
    // 裸 `DISTINCT` 已经实现(见 `ir_features.rs`);`DISTINCT ON` 仍然拒绝
    let err = failure(&mut db, "SELECT DISTINCT ON (g) g FROM t");
    assert!(err.to_string().contains("DISTINCT ON"), "{err}");
}

#[test]
fn unknown_functions_are_rejected_by_name() {
    let mut db = SymbolTable::new();
    // `ROUND` / `UPPER` / `LOWER` / `LEFT` / `LENGTH` / `CONCAT_WS` / `EXTRACT` 都已经
    // 实现了,挑一个真没有的
    let err = failure(&mut db, "SELECT TRIM('x') AS x");
    assert!(err.to_string().contains("trim"), "{err}");
}

#[test]
fn star_in_a_grouped_query_is_rejected_with_a_hint() {
    let mut db = db(FIXTURE);
    let err = failure(&mut db, "SELECT * FROM t GROUP BY g");
    assert!(err.to_string().contains("GROUP BY"), "{err}");
}

#[test]
fn coalesce_can_be_a_group_key() {
    // `GROUP BY COALESCE(...)`(tests.sql 的 14.3 那个形状):分组要真按那个表达式分,
    // 不是只在投影里算一遍.NULL 单独成一组.
    let mut db = db(
        "CREATE TABLE u (k INT, v INT);
         INSERT INTO u VALUES (1, 1), (1, 2), (NULL, 3), (2, 4);",
    );

    let table = q(&mut db, "SELECT COALESCE(k, -1) AS key, COUNT(*) AS n FROM u GROUP BY COALESCE(k, -1) ORDER BY key");
    assert_eq!(pairs(&table, 0, 1), vec!["-1=1", "1=2", "2=1"]);

    // 反向对照:投影里的表达式与分组键**不同**就得拒绝(`k` 既不是分组键,
    // 也没被聚合包住).这条能证明上面那条真的把 COALESCE 认成了分组键--
    // 否则它就只是"投影里另算了一遍"
    let err = failure(&mut db, "SELECT k, COUNT(*) FROM u GROUP BY COALESCE(k, -1)");
    assert!(err.to_string().contains("GROUP BY"), "{err}");
}

#[test]
fn coalesce_over_an_aggregate_is_still_a_grouped_query() {
    // `COALESCE(COUNT(*), 0)` 的参数里有聚合 -- "是不是分组查询"要判对,
    // 否则这个聚合会被当成"出现在了不能出现的位置"
    let mut db = db(FIXTURE);
    assert_eq!(one(&mut db, "SELECT COALESCE(COUNT(*), 0) FROM t"), "5");
}

// ------------------------------------- `COUNT(*)` 无 `WHERE` 时问 `nrows()`(3.8)

/// 一张会数数的 `t(x INT)`:整行读(`row`)与单格读(`value`)分开数.
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    rows_read: Rc<Cell<usize>>,
}

impl CountingTable {
    fn new(values: &[i32], rows_read: &Rc<Cell<usize>>) -> Self {
        CountingTable {
            shape: ColumnsShape::new(vec![Column::new("x", DataType::Int)]),
            rows: values.iter().map(|v| vec![Value::Int(*v)]).collect(),
            rows_read: Rc::clone(rows_read),
        }
    }
}

impl TabShape for CountingTable {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Table for CountingTable {
    fn name(&self) -> &str {
        "t"
    }

    fn nrows(&self) -> usize {
        self.rows.len()
    }

    fn row(&self, r: usize) -> Option<Vec<Value>> {
        self.rows_read.set(self.rows_read.get() + 1);
        self.rows.get(r).cloned()
    }

    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.rows.get(r).and_then(|row| row.get(c)).cloned()
    }
}

/// 一张 100 行的计数表(行数够大:老写法会读 100 次 `row`).
fn counting() -> (SymbolTable, Rc<Cell<usize>>) {
    let rows_read = Rc::new(Cell::new(0));
    let mut symbols = SymbolTable::new();
    let values: Vec<i32> = (0..100).collect();
    symbols
        .define("t", CountingTable::new(&values, &rows_read))
        .expect("建 t");
    (symbols, rows_read)
}

#[test]
fn count_star_without_a_where_asks_the_row_count() {
    let (mut symbols, rows_read) = counting();

    // 答案就是 `nrows()`
    assert_eq!(one(&mut symbols, "SELECT COUNT(*) AS n FROM t"), "100");
    assert_eq!(rows_read.get(), 0, "一行都不该被读");

    // 计划里没有 `Aggregate`,也没有 `Scan`
    let plan = ydbms::ir::compile("SELECT COUNT(*) FROM t", &symbols).expect("降级");
    let rendered = plan.explain(&symbols).expect("渲染");
    assert!(rendered.contains("CountRows(t)"), "{rendered}");
    assert!(!rendered.contains("Aggregate"), "{rendered}");
    assert!(!rendered.contains("Scan"), "{rendered}");

    // 权限那一半:`COUNT(*)` 一列都没读,但**那张表仍旧报在计划里**
    // (空集合 = "这张表上至少有一列的 SELECT",见 `tests/ir_columns.rs`)
    let used = plan.used_columns(&symbols).expect("列使用分析");
    assert_eq!(used.get("t"), Some(&Some(BTreeSet::new())), "{used:?}");
}

#[test]
fn count_star_with_a_where_still_scans() {
    let (mut symbols, rows_read) = counting();

    // 有 `WHERE` 就得先筛:那是 `Aggregate` 那条路,行为不变
    assert_eq!(one(&mut symbols, "SELECT COUNT(*) AS n FROM t WHERE x > 49"), "50");
    assert!(rows_read.get() > 0, "筛过一遍:{} 次整行读", rows_read.get());

    let plan = ydbms::ir::compile("SELECT COUNT(*) FROM t WHERE x > 49", &symbols).expect("降级");
    let rendered = plan.explain(&symbols).expect("渲染");
    assert!(rendered.contains("Aggregate"), "{rendered}");
}

#[test]
fn count_of_something_else_is_not_the_row_count() {
    let (mut symbols, rows_read) = counting();

    // `COUNT(x)` 要跳过 NULL,`COUNT(DISTINCT x)` 要去重,`SUM(x)` 要读值 --
    // 都不是"行数",一条都不许走那条捷径
    for sql in [
        "SELECT COUNT(x) AS n FROM t",
        "SELECT COUNT(DISTINCT x) AS n FROM t",
        "SELECT SUM(x) AS n FROM t",
        "SELECT AVG(x) AS n FROM t",
    ] {
        let plan = ydbms::ir::compile(sql, &symbols).expect("降级");
        let rendered = plan.explain(&symbols).expect("渲染");
        assert!(rendered.contains("Aggregate"), "{sql}:{rendered}");
        one(&mut symbols, sql);
    }
    assert!(rows_read.get() > 0, "这些聚合真的要读值");
}
