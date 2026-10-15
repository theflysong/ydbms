//! `Plan::used_columns` 的 testbench(6.4 列级授权的地基).
//!
//! 列级授权要问的是"这条查询**读到了**哪些列",而这只有从计划往回推才算得出来
//! (计划里的列下标是相对每个算子的输入 schema 的).这里把各种形状各钉一条:
//! 投影,`SELECT *`,一列都不读,连接,派生表,`DISTINCT`,`UNION [ALL]`,聚合,
//! 相关子查询(保守地按整表算).
//!
//! 口径是**宁可多要**:少要一次就是一次安静的越权读,多要一次只是多一句"没权限".

use std::collections::{BTreeMap, BTreeSet};

use ydbms::db::table::{Column, ColumnsShape, DataType};
use ydbms::db::PhysTab;
use ydbms::ir::{compile, SymbolTable};

/// 两张表:`t(a INT, b TEXT, c INT)`,`u(x INT, y TEXT)`.
fn catalog() -> SymbolTable {
    let mut symbols = SymbolTable::new();

    let t = ColumnsShape::new(vec![
        Column::new("a", DataType::Int),
        Column::new("b", DataType::Text),
        Column::new("c", DataType::Int),
    ]);
    symbols.define("t", PhysTab::new("t", &t)).expect("建 t");

    let u = ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("y", DataType::Text),
    ]);
    symbols.define("u", PhysTab::new("u", &u)).expect("建 u");

    symbols
}

/// 一条查询读到了哪些列:`None` = 整张表(保守),`Some` = 具体的那几列.
fn used(sql: &str) -> BTreeMap<String, Option<BTreeSet<String>>> {
    let symbols = catalog();
    let plan = compile(sql, &symbols).expect("降级");
    plan.used_columns(&symbols).expect("列使用分析")
}

/// 具体的那几列(断言用).
fn columns(sql: &str, table: &str) -> Option<Vec<String>> {
    match used(sql).get(table) {
        Some(Some(columns)) => Some(columns.iter().cloned().collect()),
        Some(None) => None,
        None => panic!("{sql} 里没有扫到 {table}"),
    }
}

#[test]
fn a_plain_projection_reads_only_what_it_selects() {
    assert_eq!(columns("SELECT a FROM t", "t"), Some(vec!["a".to_string()]));
    assert_eq!(
        columns("SELECT b, a FROM t", "t"),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    // `WHERE` 里的列也算读到
    assert_eq!(
        columns("SELECT a FROM t WHERE c > 1", "t"),
        Some(vec!["a".to_string(), "c".to_string()])
    );
}

#[test]
fn select_star_reads_every_column() {
    // 根就是那张表(整表),所以走的是保守的那一支:`None` = 这张表的全部列
    assert_eq!(columns("SELECT * FROM t", "t"), None);
    // 上层再投影一层也还是整表(每一列都往外流)
    assert_eq!(columns("SELECT * FROM (SELECT * FROM t) AS d", "t"), None);
}

#[test]
fn a_query_that_reads_no_column_still_touches_the_table() {
    // `SELECT 1 FROM t`:一列都没读,但表被扫了--空集合,不是"没有这条记录"
    assert_eq!(columns("SELECT 1 FROM t", "t"), Some(Vec::new()));
}

#[test]
fn a_join_splits_the_demand_by_side() {
    let used = used("SELECT t.a, u.y FROM t, u WHERE t.c = u.x");
    assert_eq!(
        used.get("t"),
        Some(&Some(BTreeSet::from(["a".to_string(), "c".to_string()])))
    );
    assert_eq!(
        used.get("u"),
        Some(&Some(BTreeSet::from(["x".to_string(), "y".to_string()])))
    );
}

#[test]
fn a_derived_table_only_demands_the_columns_that_flow_out() {
    // 派生表里选了 `a, b`,但外层只要 `a`:里面那个 `b` 就不该被算上
    let used = used("SELECT a FROM (SELECT a, b FROM t) AS d");
    assert_eq!(
        used.get("t"),
        Some(&Some(BTreeSet::from(["a".to_string()])))
    );
}

#[test]
fn distinct_and_dedup_union_demand_their_input_row() {
    // 去重要看**输入那一行**的全部列;输入是投影,所以落到表上还是精确的那几列
    assert_eq!(
        columns("SELECT DISTINCT a FROM t", "t"),
        Some(vec!["a".to_string()])
    );
    assert_eq!(
        columns("SELECT DISTINCT a, b FROM t", "t"),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(
        columns("SELECT a FROM t UNION SELECT x FROM u", "t"),
        Some(vec!["a".to_string()])
    );
    assert_eq!(
        columns("SELECT a FROM t UNION SELECT x FROM u", "u"),
        Some(vec!["x".to_string()])
    );
}

#[test]
fn union_all_keeps_the_precise_demand() {
    let used = used("SELECT a FROM t UNION ALL SELECT x FROM u");
    assert_eq!(used.get("t"), Some(&Some(BTreeSet::from(["a".to_string()]))));
    assert_eq!(used.get("u"), Some(&Some(BTreeSet::from(["x".to_string()]))));
}

#[test]
fn an_aggregate_demands_what_it_actually_reads() {
    // `count(*)` 没有参数:一列都不读(空集合),留给权限检查按"至少一列"处理
    assert_eq!(columns("SELECT COUNT(*) FROM t", "t"), Some(Vec::new()));
    // 带参数的聚合读那一列
    assert_eq!(
        columns("SELECT SUM(c) FROM t", "t"),
        Some(vec!["c".to_string()])
    );
    // 分组键也算读
    assert_eq!(
        columns("SELECT b, COUNT(*) FROM t GROUP BY b", "t"),
        Some(vec!["b".to_string()])
    );
}

#[test]
fn a_scalar_subquery_reads_its_own_table() {
    let used = used("SELECT a, (SELECT MAX(x) FROM u) FROM t");
    assert_eq!(used.get("t"), Some(&Some(BTreeSet::from(["a".to_string()]))));
    assert_eq!(used.get("u"), Some(&Some(BTreeSet::from(["x".to_string()]))));
}

#[test]
fn a_correlated_subquery_demands_the_whole_row_conservatively() {
    // 相关子查询读外层行走的是 `Step::Outer`,`Prog::columns()` 看不到它--
    // 所以外层那张表按**整表**算(保守).内层自己的表照旧精确.
    let used = used("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.c)");
    assert_eq!(used.get("t"), Some(&None));
    assert_eq!(used.get("u"), Some(&Some(BTreeSet::from(["x".to_string()]))));
}

#[test]
fn an_index_path_counts_as_reading_its_column() {
    // 索引路径本身要读那一列(`WHERE c = 1` 里 `c` 已经被引用了,这里换个不引用它的写法)
    let mut symbols = catalog();
    let script = ydbms::ir::Script::parse("CREATE INDEX i_c ON t (c);").expect("解析");
    script.run_one(0, &mut symbols);

    // `SELECT a FROM t` 不走索引;换成带谓词的写法,索引才可能被挑中
    let plan = compile("SELECT a FROM t WHERE c = 1", &symbols).expect("降级");
    let used = plan.used_columns(&symbols).expect("列使用分析");
    assert_eq!(
        used.get("t"),
        Some(&Some(BTreeSet::from(["a".to_string(), "c".to_string()])))
    );
}
