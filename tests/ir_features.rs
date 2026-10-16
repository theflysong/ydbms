//! TODO 1.4 的查询层特性:`IN` 列表,`BETWEEN`,`LIKE`/`ILIKE`,`~*`,`DISTINCT`,
//! `COUNT(*) FILTER`,`::numeric`,`CURRENT_DATE`/`EXTRACT` 与几个标量函数.
//!
//! 全部用**小合成表**,期望值是手算的--断言的是"算得对不对",不是"和我们自己的
//! 实现一致".这个文件里每条查询的形状都能在 `ydbms-test/` 的 `tests.sql` /
//! `correctness/queries.sql` 里找到对应(那些脚本要等 1.2/1.3 把 filmdb 建起来).
//!
//! 这些语义都逐条对照过 **PostgreSQL 18.6**.

use ydbms::db::table::{DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::irgen::civil_date_from_days;
use ydbms::ir::{compile, execute, run_script, Outcome, SymbolTable};

/// ```text
/// id | name      | score | joined                  | 备注
/// 1  | Alice     |  1.50 | 2023-01-02 00:00:00     |
/// 2  | bob       |  2.25 | 2024-03-04 00:00:00     |
/// 3  | Céline    |  3.75 | 2024-05-06 00:00:00     |
/// 4  | dave      |  NULL | 2025-07-08 00:00:00     | 分数为 NULL
/// 5  | 50%off    |  0.50 | 2025-07-08 00:00:00     | 名字里有字面 `%`
/// ```
const FIXTURE: &str = "
    CREATE TABLE t (id INT, name TEXT, score NUMERIC(4,2), joined TIMESTAMP);
    INSERT INTO t VALUES (1, 'Alice',  1.50, '2023-01-02 00:00:00');
    INSERT INTO t VALUES (2, 'bob',    2.25, '2024-03-04 00:00:00');
    INSERT INTO t VALUES (3, 'Céline', 3.75, '2024-05-06 00:00:00');
    INSERT INTO t VALUES (4, 'dave',   NULL, '2025-07-08 00:00:00');
    INSERT INTO t VALUES (5, '50%off', 0.50, '2025-07-08 00:00:00');
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

/// 跑一条查询,拿结果表.
fn q(symbols: &SymbolTable, sql: &str) -> PhysTab {
    let plan = compile(sql, symbols).unwrap_or_else(|e| panic!("`{sql}` 降级失败:{e}"));
    execute(&plan, symbols).unwrap_or_else(|e| panic!("`{sql}` 执行失败:{e}"))
}

/// 跑一条查询,把第 `column` 列的整数取出来.
fn ints(symbols: &SymbolTable, sql: &str, column: usize) -> Vec<i32> {
    let out = q(symbols, sql);
    (0..out.nrows())
        .map(|r| {
            out.value(r, column)
                .and_then(|v| v.as_int())
                .unwrap_or_else(|| panic!("第 {r} 行第 {column} 列不是整数"))
        })
        .collect()
}

/// 跑一条查询,把第 `column` 列取成文本.
fn texts(symbols: &SymbolTable, sql: &str, column: usize) -> Vec<String> {
    let out = q(symbols, sql);
    (0..out.nrows())
        .map(|r| {
            out.value(r, column)
                .unwrap_or_else(|| panic!("第 {r} 行第 {column} 列没有值"))
                .to_string()
        })
        .collect()
}

/// 跑一条查询,期望它被**显式拒绝**.
fn rejected(symbols: &SymbolTable, sql: &str) -> DbError {
    match compile(sql, symbols) {
        Err(e) => e,
        Ok(plan) => match execute(&plan, symbols) {
            Err(e) => e,
            Ok(table) => panic!("`{sql}` 本应被拒绝,却返回了 {} 行", table.nrows()),
        },
    }
}

// ------------------------------------------------------------------ IN 列表

#[test]
fn in_list_behaves_like_a_chain_of_equalities() {
    let symbols = db();
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id IN (1, 3, 5) ORDER BY id", 0), vec![1, 3, 5]);
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id IN (2) ORDER BY id", 0), vec![2]);
    assert_eq!(
        ints(&symbols, "SELECT id FROM t WHERE id IN (1, 3) OR id IN (2, 5) ORDER BY id", 0),
        vec![1, 2, 3, 5]
    );
}

#[test]
fn not_in_with_a_null_in_the_list_is_never_true() {
    // SQL 的三值逻辑:`x NOT IN (1, NULL)` 永远是 UNKNOWN(`x <> NULL` 是 UNKNOWN)
    let symbols = db();
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id NOT IN (1, NULL)", 0).len(), 0);
    // 列表里没有 NULL 时,`NOT IN` 就是"不等于每一个"
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id NOT IN (1, 2) ORDER BY id", 0), vec![3, 4, 5]);
}

#[test]
fn in_list_uses_the_column_type_for_the_literals() {
    // 右侧是无类型字符串,用左边的 TIMESTAMP 列定型(`joined IN ('2024-03-04 00:00:00')`)
    let symbols = db();
    assert_eq!(
        ints(&symbols, "SELECT id FROM t WHERE joined IN ('2024-03-04 00:00:00')", 0),
        vec![2]
    );
}

// ------------------------------------------------------------------ BETWEEN

#[test]
fn between_is_a_closed_interval() {
    let symbols = db();
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id BETWEEN 2 AND 4 ORDER BY id", 0), vec![2, 3, 4]);
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id NOT BETWEEN 2 AND 4 ORDER BY id", 0), vec![1, 5]);
    // 边界要**含**在里面
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id BETWEEN 1 AND 1", 0), vec![1]);
}

#[test]
fn between_on_numeric_columns_uses_numeric_comparison() {
    // `1.5` 与 `2.25` 之间只有 2;按文本比会得到完全不同的答案
    let symbols = db();
    assert_eq!(
        ints(&symbols, "SELECT id FROM t WHERE score BETWEEN 1.5 AND 2.25 ORDER BY id", 0),
        vec![1, 2]
    );
}

// --------------------------------------------------------------- LIKE / 正则

#[test]
fn like_matches_percent_and_underscore() {
    let symbols = db();
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name LIKE 'A%' ORDER BY id", 0), vec!["Alice"]);
    // `_` 是**一个**字符
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name LIKE '_ob' ORDER BY id", 0), vec!["bob"]);
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name LIKE '%e' ORDER BY id", 0), vec!["Alice", "Céline", "dave"]);
}

#[test]
fn like_can_escape_the_wildcards() {
    let symbols = db();
    // `\%` 是字面的百分号:只有 '50%off' 匹配
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name LIKE '50\\%off'", 0), vec!["50%off"]);
    // 不转义的 `%` 是通配符,于是两个都匹配
    assert_eq!(
        texts(&symbols, "SELECT name FROM t WHERE name LIKE 'A%' OR name LIKE '50%' ORDER BY id", 0),
        vec!["Alice", "50%off"]
    );
}

#[test]
fn ilike_ignores_case_and_not_ilike_negates_it() {
    let symbols = db();
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name ILIKE 'BOB'", 0), vec!["bob"]);
    assert_eq!(
        texts(&symbols, "SELECT name FROM t WHERE name ILIKE 'a%' ORDER BY id", 0),
        vec!["Alice"]
    );
    // `NOT ILIKE` 只把"真"翻过来;NULL 仍然是 NULL(这里 name 都不为 NULL)
    assert_eq!(
        texts(&symbols, "SELECT name FROM t WHERE name NOT ILIKE '%o%' ORDER BY id", 0),
        vec!["Alice", "Céline", "dave"]
    );
}

#[test]
fn ilike_is_case_insensitive_on_unicode_too() {
    let symbols = db();
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name ILIKE 'céline'", 0), vec!["Céline"]);
}

#[test]
fn regex_match_supports_posix_word_boundaries() {
    let symbols = db();
    // `\m` / `\M` 是 PostgreSQL 的词边界写法(我们把它翻成 `\b`)
    assert_eq!(
        texts(&symbols, "SELECT name FROM t WHERE name ~* '\\mCéline\\M'", 0),
        vec!["Céline"]
    );
    assert_eq!(
        texts(&symbols, "SELECT name FROM t WHERE name ~* '^a' ORDER BY id", 0),
        vec!["Alice"]
    );
    // `~*` 忽略大小写,`~` 不忽略
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name ~ 'BOB'", 0).len(), 0);
    assert_eq!(texts(&symbols, "SELECT name FROM t WHERE name ~* 'bob'", 0), vec!["bob"]);
}

#[test]
fn a_broken_regex_is_rejected_at_run_time() {
    let symbols = db();
    let err = rejected(&symbols, "SELECT id FROM t WHERE name ~* '['");
    assert!(err.to_string().contains("正则"), "{err}");
}

// ----------------------------------------------------------------- DISTINCT

#[test]
fn distinct_removes_duplicate_output_rows() {
    let symbols = db();
    // `joined` 有重复(4 与 5 同一天)→ 4 个不同的值
    assert_eq!(texts(&symbols, "SELECT DISTINCT joined FROM t ORDER BY joined", 0).len(), 4);
    // 名字各不相同 → 5 个
    assert_eq!(texts(&symbols, "SELECT DISTINCT name FROM t ORDER BY name", 0).len(), 5);
    // 分数是 1.5 / 2.25 / 3.75 / 0.5 外加一个 NULL:**NULL 也算一个不同的值** → 5 个
    assert_eq!(texts(&symbols, "SELECT DISTINCT score FROM t ORDER BY score", 0).len(), 5);
}

#[test]
fn distinct_works_with_qualified_order_by_keys() {
    // `ydbms-test/tests.sql` 的 11.1 / 12.1 就是这个形状:DISTINCT + 限定列名排序
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT DISTINCT t.name, t.id FROM t ORDER BY t.id DESC",
    );
    assert_eq!(out.nrows(), 5);
    assert_eq!(
        texts(&symbols, "SELECT DISTINCT t.name, t.id FROM t ORDER BY t.id DESC", 0),
        vec!["50%off", "dave", "Céline", "bob", "Alice"]
    );
}

// ------------------------------------------------------- COUNT(*) FILTER

#[test]
fn count_star_filter_counts_only_the_matching_rows() {
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT COUNT(*) AS all_rows,
                COUNT(*) FILTER (WHERE score >= 2) AS big,
                COUNT(*) FILTER (WHERE name ILIKE 'a%') AS a_names
         FROM t",
    );

    // 手算:5 行;`score >= 2` 命中 2.25,3.75(NULL 不命中)→ 2;名字以 a/A 开头只有 Alice → 1
    assert_eq!(out.value(0, 0), Some(Value::Int(5)));
    assert_eq!(out.value(0, 1), Some(Value::Int(2)));
    assert_eq!(out.value(0, 2), Some(Value::Int(1)));
}

#[test]
fn sum_filter_skips_the_rows_the_predicate_rejects() {
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT SUM(score) FILTER (WHERE score < 3) AS small,
                SUM(score) AS total
         FROM t",
    );

    // 手算:`score < 3` 命中 1.5,2.25,0.5 → 4.25;总和 1.5+2.25+3.75+0.5 = 8
    assert_eq!(out.value(0, 0).map(|v| v.to_string()), Some("4.25".to_string()));
    assert_eq!(out.value(0, 1).map(|v| v.to_string()), Some("8".to_string()));
}

// ------------------------------------------------------------------- 造型

#[test]
fn numeric_cast_turns_integers_into_decimals() {
    let symbols = db();
    // `id::numeric / 2` 必须走十进制除法(整数除法会给 0)
    let out = q(&symbols, "SELECT id::numeric / 2 AS half FROM t WHERE id = 3");
    assert_eq!(out.value(0, 0).map(|v| v.to_string()), Some("1.5".to_string()));

    // 整数除法本身仍然是整数除法(对照:PostgreSQL 里也是 3/2 = 1)
    let out = q(&symbols, "SELECT id / 2 AS whole FROM t WHERE id = 3");
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));
}

#[test]
fn unsupported_casts_are_rejected() {
    let symbols = db();
    // 值模型里没有意义的组合仍在**降级期**拒绝:时间戳不能直接当整数用,整数也不能
    // 直接当日期用(PostgreSQL 也拒绝这两条).`DATE` 与 `TIMESTAMP` 互转同样是拒绝的
    // --那是记录在案的偏差(`db::table` 的 `TypeFamily` 文档里写着).
    for sql in ["SELECT joined::int FROM t", "SELECT id::date FROM t"] {
        let err = rejected(&symbols, sql);
        assert!(err.to_string().contains("不能把"), "`{sql}`:{err}");
    }
}

#[test]
fn lower_folds_case() {
    // 期望值对照过 PostgreSQL 18.6
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT LOWER('AbC') AS l, LOWER(NULL) AS nl, LOWER(name) AS lower_name
         FROM t WHERE id = 3",
    );
    assert_eq!(out.value(0, 0), Some(Value::Text("abc".to_string())));
    assert_eq!(out.value(0, 1), Some(Value::Null));
    // 第 3 行的 name 是 'Céline':按字符折,不按字节
    assert_eq!(out.value(0, 2), Some(Value::Text("céline".to_string())));

    // 非文本参数照样拒绝(PostgreSQL 也是 `function lower(integer) does not exist`)
    let err = rejected(&symbols, "SELECT LOWER(id) FROM t");
    assert!(err.to_string().contains("必须是 TEXT"), "{err}");
}

// ------------------------------------------------------------------ CAST

#[test]
fn casting_to_text_keeps_the_rendered_form() {
    let symbols = db();

    let out = q(
        &symbols,
        "SELECT CAST(1994 AS TEXT) AS lit,
                CAST(id AS TEXT) AS id_text,
                CAST(score AS TEXT) AS score_text,
                CAST(joined AS TEXT) AS joined_text,
                CAST(NULL AS TEXT) AS nothing,
                CAST(name AS TEXT) AS name_text
         FROM t WHERE id = 1",
    );
    assert_eq!(out.value(0, 0), Some(Value::Text("1994".to_string())));
    assert_eq!(out.value(0, 1), Some(Value::Text("1".to_string())));
    // `NUMERIC(4,2)` 的 `1.50` 规范化成 `1.5`:这是 `Value::Numeric` 的不变量
    // (PostgreSQL 会原样保留 `1.50`,那条偏差写在 `db::table` 里)
    assert_eq!(out.value(0, 2), Some(Value::Text("1.5".to_string())));
    assert_eq!(
        out.value(0, 3),
        Some(Value::Text("2023-01-02 00:00:00.000000".to_string()))
    );
    assert_eq!(out.value(0, 4), Some(Value::Null));
    // 同族恒等
    assert_eq!(out.value(0, 5), Some(Value::Text("Alice".to_string())));

    // **呈现与渲染逐字相同**:`CAST(9.5 AS TEXT)` 与 `SELECT 9.5` 渲染出来的字面
    // 一模一样(类型当然不同:一个是 `TEXT`,一个是 `NUMERIC`)
    let casted = q(&symbols, "SELECT CAST(9.5 AS TEXT) AS x");
    let rendered = q(&symbols, "SELECT 9.5 AS x");
    assert_eq!(
        casted.value(0, 0).unwrap().to_string(),
        rendered.value(0, 0).unwrap().to_string()
    );
    assert_eq!(casted.value(0, 0), Some(Value::Text("9.5".to_string())));
    assert_eq!(rendered.value(0, 0), Some(Value::Numeric("9.5".to_string())));
}

#[test]
fn casting_text_to_numbers_and_dates_parses_the_value_at_run_time() {
    // 字面量走"按目标类型定型字面量"那条路(`cast_literal_to_int` 已经盯着);
    // 这里要的是**列**上那一条:类型上放行,值到运行期才解析.
    let mut symbols = db();
    for sql in [
        "CREATE TABLE good (code TEXT, num TEXT, day TEXT, ts TEXT)",
        "INSERT INTO good VALUES ('0042', '9.5', '2024-01-15', '2024-01-15 08:30:00')",
        "CREATE TABLE bad (code TEXT, num TEXT, day TEXT, ts TEXT)",
        "INSERT INTO bad VALUES ('abc', 'abc', 'not-a-date', 'nope')",
    ] {
        for r in run_script(sql, &mut symbols).expect("应当能解析") {
            assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
        }
    }

    let out = q(
        &symbols,
        "SELECT CAST(code AS INT) AS a,
                CAST(num AS NUMERIC) AS b,
                CAST(day AS DATE) AS c,
                CAST(ts AS TIMESTAMP) AS d
         FROM good",
    );
    assert_eq!(out.value(0, 0), Some(Value::Int(42)));
    assert_eq!(out.value(0, 1), Some(Value::Numeric("9.5".to_string())));
    assert_eq!(out.value(0, 2), Some(Value::Date("2024-01-15".to_string())));
    assert_eq!(
        out.value(0, 3),
        Some(Value::Timestamp("2024-01-15 08:30:00.000000".to_string()))
    );

    // 解析不了的是**运行期**错误(PostgreSQL 也一样:`invalid input syntax for
    // type integer: "abc"`),而且只可能是这个值的错--类型那一关已经过了
    for sql in [
        "SELECT CAST(code AS INT) FROM bad",
        "SELECT CAST(num AS NUMERIC) FROM bad",
        "SELECT CAST(day AS DATE) FROM bad",
        "SELECT CAST(ts AS TIMESTAMP) FROM bad",
    ] {
        let err = rejected(&symbols, sql);
        assert!(err.to_string().contains("看不懂"), "`{sql}`:{err}");
    }
}
// ------------------------------------------------------- 函数与 CURRENT_DATE

#[test]
fn upper_left_and_concat_ws_work_like_postgresql() {
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT UPPER('abc') AS u,
                LEFT('abcdef', 3) AS l,
                LEFT('abcdef', -2) AS ln,
                CONCAT_WS('-', 'a', NULL, 'b') AS cw,
                CONCAT_WS(NULL, 'a', 'b') AS null_sep,
                LEFT(NULL, 2) AS null_text",
    );

    assert_eq!(out.value(0, 0), Some(Value::Text("ABC".to_string())));
    assert_eq!(out.value(0, 1), Some(Value::Text("abc".to_string())));
    // PostgreSQL:`n` 为负时去掉右边 `|n|` 个字符
    assert_eq!(out.value(0, 2), Some(Value::Text("abcd".to_string())));
    // NULL 参数**跳过**,分隔符为 NULL 才是 NULL
    assert_eq!(out.value(0, 3), Some(Value::Text("a-b".to_string())));
    assert_eq!(out.value(0, 4), Some(Value::Null));
    assert_eq!(out.value(0, 5), Some(Value::Null));
}

#[test]
fn concat_builds_text_from_text_and_propagates_null() {
    // 期望值逐条对照过 PostgreSQL 18.6
    let symbols = db();

    let out = q(
        &symbols,
        "SELECT 'a' || 'b' AS ab,
                'a' || 'b' || 'c' AS abc,
                NULL || 'a' AS n_left,
                'a' || NULL AS n_right,
                name || '!' AS on_a_column
         FROM t WHERE id = 1",
    );
    assert_eq!(out.value(0, 0), Some(Value::Text("ab".to_string())));
    assert_eq!(out.value(0, 1), Some(Value::Text("abc".to_string())));
    // NULL 传播:任一侧是 NULL,结果就是 NULL
    assert_eq!(out.value(0, 2), Some(Value::Null));
    assert_eq!(out.value(0, 3), Some(Value::Null));
    assert_eq!(out.value(0, 4), Some(Value::Text("Alice!".to_string())));
}

#[test]
fn concat_refuses_a_non_text_side() {
    // 已知偏差:PostgreSQL 有 `text || anynonarray`,于是那边 `'a' || 1` 是 `a1`;
    // 本版只认"两侧都是文本",这里拒绝.口径写在 `BinOp::Concat` 的文档里.
    let symbols = db();
    for sql in ["SELECT 'a' || 1", "SELECT 1 || 'a'", "SELECT 1 || 2"] {
        let err = rejected(&symbols, sql);
        assert!(err.to_string().contains("必须是 TEXT"), "`{sql}`:{err}");
    }
}

#[test]
fn coalesce_picks_the_first_non_null() {
    // 期望值逐条对照过 PostgreSQL 18.6
    let symbols = db();
    let out = q(
        &symbols,
        "SELECT COALESCE(NULL, 'a') AS a,
                COALESCE(NULL, NULL) AS nn,
                COALESCE('a', 'b') AS ab,
                COALESCE(score, 0) AS s
         FROM t WHERE id = 4",
    );
    assert_eq!(out.value(0, 0), Some(Value::Text("a".to_string())));
    assert_eq!(out.value(0, 1), Some(Value::Null));
    assert_eq!(out.value(0, 2), Some(Value::Text("a".to_string())));
    // 第 4 行的 score 是 NULL → 取 0(一侧是 NUMERIC,按 `CASE` 那条规则放大)
    assert_eq!(out.value(0, 3), Some(Value::Numeric("0".to_string())));
}

#[test]
fn coalesce_unifies_argument_types_like_case() {
    let symbols = db();

    // 两侧都是 INT 就不放大(`COALESCE(id, 0)` 的类型仍是 INT)
    let out = q(&symbols, "SELECT COALESCE(id, 0) AS x FROM t WHERE id = 1");
    assert_eq!(out.columns()[0].ty, DataType::Int);
    assert_eq!(out.value(0, 0), Some(Value::Int(1)));

    // INT 与 NUMERIC 统一成 NUMERIC,取到 INT 那一侧要真的转
    // (PostgreSQL 18.6 上 `SELECT COALESCE(NULL, 1, 2.5)` 就是 `1`,类型 numeric)
    let out = q(&symbols, "SELECT COALESCE(NULL, 1, 2.5) AS x");
    assert_eq!(out.columns()[0].ty, DataType::NumericAny);
    assert_eq!(out.value(0, 0), Some(Value::Numeric("1".to_string())));

    let out = q(&symbols, "SELECT COALESCE(NULL, 2.5, 1) AS x");
    assert_eq!(out.value(0, 0), Some(Value::Numeric("2.5".to_string())));

    // 统一不了的组合要拒绝(PostgreSQL 也说 COALESCE 的 text 与 integer 对不上)
    let err = rejected(&symbols, "SELECT COALESCE(NULL, 'a', 1)");
    assert!(err.to_string().contains("COALESCE"), "{err}");
}

#[test]
fn extract_year_reads_the_first_four_digits() {
    let symbols = db();
    // 入学年份(`enrolled_at` 是定宽 TIMESTAMP 文本,取前 4 位即可)
    assert_eq!(
        ints(&symbols, "SELECT EXTRACT(YEAR FROM joined) AS y FROM t ORDER BY id", 0),
        vec![2023, 2024, 2024, 2025, 2025]
    );
    // 排序键里的 `EXTRACT` 也能用(query5 [5-4] 的形状)
    let out = q(
        &symbols,
        "SELECT id FROM t ORDER BY EXTRACT(YEAR FROM joined) DESC, id LIMIT 2",
    );
    assert_eq!(out.value(0, 0), Some(Value::Int(4)));
    assert_eq!(out.value(1, 0), Some(Value::Int(5)));
}

#[test]
fn only_the_year_field_is_supported() {
    let symbols = db();
    let err = rejected(&symbols, "SELECT EXTRACT(MONTH FROM joined) FROM t");
    assert!(err.to_string().contains("YEAR"), "{err}");
}

#[test]
fn current_date_is_a_canonical_date_text() {
    let symbols = db();
    let out = q(&symbols, "SELECT CURRENT_DATE AS today, EXTRACT(YEAR FROM CURRENT_DATE) AS y");
    // TODO 1.3 之后它就是 `DATE` 列(不再是"记录在案的 TEXT 偏差")
    assert_eq!(out.columns()[0].ty, DataType::Date, "CURRENT_DATE 的列类型");
    let today = out.value(0, 0).expect("CURRENT_DATE").to_string();

    // 形状必须是 `YYYY-MM-DD`--它是**规范文本**(字典序 == 时间序)
    let parts: Vec<&str> = today.split('-').collect();
    assert_eq!(parts.len(), 3, "不能解析成日期:{today}");
    assert_eq!(parts[0].len(), 4);
    assert_eq!(parts[1].len(), 2);
    assert_eq!(parts[2].len(), 2);

    let year: i32 = parts[0].parse().expect("年份");
    assert_eq!(out.value(0, 1), Some(Value::Int(year)));
    assert!(year >= 2024, "时钟不可能在这之前:{year}");
}

#[test]
fn civil_date_from_days_matches_known_dates() {
    // 纪元与几个好记的日子;实现是纯整数运算,所以这些断言是确定性的
    assert_eq!(civil_date_from_days(0), (1970, 1, 1));
    assert_eq!(civil_date_from_days(1), (1970, 1, 2));
    assert_eq!(civil_date_from_days(365), (1971, 1, 1));
    // 2000-03-01 之前的闰年边界
    assert_eq!(civil_date_from_days(11_016), (2000, 2, 29));
    assert_eq!(civil_date_from_days(20_000), (2024, 10, 4));
}

// ------------------------------------------------------------------- 该拒绝的

#[test]
fn distinct_on_is_rejected() {
    let symbols = db();
    let err = rejected(&symbols, "SELECT DISTINCT ON (id) id FROM t");
    assert!(err.to_string().contains("DISTINCT ON"), "{err}");
}

#[test]
fn intersect_and_except_are_rejected() {
    let symbols = db();
    for sql in [
        "SELECT id FROM t INTERSECT SELECT id FROM t",
        "SELECT id FROM t EXCEPT SELECT id FROM t",
    ] {
        let err = rejected(&symbols, sql);
        assert!(err.to_string().contains("集合运算"), "{sql}: {err}");
    }
}

#[test]
fn union_all_keeps_duplicates() {
    let symbols = db();
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id <= 2 UNION ALL SELECT id FROM t WHERE id <= 2 ORDER BY 1", 0).len(), 4);
    assert_eq!(ints(&symbols, "SELECT id FROM t WHERE id <= 2 UNION SELECT id FROM t WHERE id <= 2 ORDER BY 1", 0), vec![1, 2]);
}

#[test]
fn like_on_a_non_text_column_is_rejected() {
    let symbols = db();
    let err = rejected(&symbols, "SELECT id FROM t WHERE id LIKE '1'");
    assert!(err.to_string().contains("TEXT"), "{err}");
}

#[test]
fn plan_schema_of_a_numeric_cast_is_numeric() {
    let symbols = db();
    let plan = compile("SELECT id::numeric FROM t", &symbols).expect("应当能降级");
    let schema = plan.schema(&symbols).expect("应当能推 schema");

    assert_eq!(schema[0].ty, DataType::NumericAny);
}

#[test]
fn a_script_with_feature_queries_runs_through_run_script() {
    // 脚本那条路(`run_script`)也要能跑这些写法,不只是 `compile` + `execute`
    let mut symbols = db();
    let results = run_script(
        "SELECT DISTINCT name FROM t WHERE id BETWEEN 1 AND 3 ORDER BY name;
         SELECT COUNT(*) FILTER (WHERE score > 1) FROM t;",
        &mut symbols,
    )
    .expect("应当能解析");

    let rows: Vec<usize> = results
        .iter()
        .map(|r| match &r.outcome {
            Ok(Outcome::Rows(table)) => table.nrows(),
            other => panic!("期望 Rows,得到 {other:?}"),
        })
        .collect();
    assert_eq!(rows, vec![3, 1]);
}
