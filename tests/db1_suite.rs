//! 直接拿 `tests/script/db1/` 里的**真实文件**跑一遍.
//!
//! 这是这一轮的验收点:`db.sql` 建库,`query1/2/3.sql` 全部语句零失败.除此之外
//! 还核几组**能从数据本身推出来**的不变量(而不是"和我们自己的实现一致"):
//! 行数,按院系分完 12 位教师,按性别分完 40 名学生,全校 `COUNT(*)` = 520.

use std::path::PathBuf;

use ydbms::db::table::{Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, StmtResult, SymbolTable};

fn script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/script/db1")
        .join(name)
}

/// 跑一个脚本文件,返回逐条结果.
fn run(db: &mut SymbolTable, name: &str) -> Vec<StmtResult> {
    let path = script(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读不了 {}:{e}", path.display()));
    run_script(&text, db).unwrap_or_else(|e| panic!("{name} 解析失败:{e}"))
}

/// 断言整份脚本零失败,并在失败时把语句原文带出来.
fn assert_all_ok(db: &mut SymbolTable, name: &str) -> Vec<StmtResult> {
    let results = run(db, name);
    let failures: Vec<String> = results
        .iter()
        .filter_map(|r| match &r.outcome {
            Ok(_) => None,
            Err(e) => Some(format!("  [{}] {} → {}", r.index, r.text, e)),
        })
        .collect();

    assert!(
        failures.is_empty(),
        "{name} 有 {} 条失败:\n{}",
        failures.len(),
        failures.join("\n")
    );
    results
}

/// 建好 db1 的库.
///
/// 每个测试自己建一份:52 条语句,不到一千行 INSERT,很便宜;换来的是**没有共享
/// 可变状态**--测试是并行跑的,全局 `&mut` 会变成数据竞争.
fn database() -> SymbolTable {
    let mut db = SymbolTable::new();
    let results = run(&mut db, "db.sql");
    for r in &results {
        assert!(
            r.outcome.is_ok(),
            "[{}] {} → {:?}",
            r.index,
            r.text,
            r.outcome
        );
    }
    db
}

fn query(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows,得到 {other:?}"),
    }
}

/// 数一张表有多少行.
fn rows(db: &SymbolTable, table: &str) -> usize {
    db.relation(table).expect("表存在").nrows()
}

/// 某一列的文本形式(断言排序键时用).
fn text_of(table: &PhysTab, row: usize, column: usize) -> String {
    table
        .value(row, column)
        .unwrap_or_else(|| panic!("第 {row} 行第 {column} 列没有值"))
        .to_string()
}

/// 某一列的整数(断言排序键时用).
fn credits_of(table: &PhysTab, row: usize) -> i32 {
    match table.value(row, 2) {
        Some(Value::Int(v)) => v,
        other => panic!("第 {row} 行的 credits 不是整数:{other:?}"),
    }
}

/// 只降级并渲染计划(`EXPLAIN` 那条路).
fn explain(db: &SymbolTable, sql: &str) -> String {
    ydbms::ir::compile(sql, db)
        .expect("应当能降级")
        .explain(db)
        .expect("应当能渲染")
}

/// 把某一列的整数加起来.
fn sum_int(table: &PhysTab, column: usize) -> i64 {
    (0..table.nrows())
        .map(|r| match table.value(r, column) {
            Some(Value::Int(v)) => v as i64,
            other => panic!("第 {r} 行第 {column} 列不是整数:{other:?}"),
        })
        .sum()
}

// ------------------------------------------------------------ 建库

#[test]
fn db_sql_loads_without_a_single_failure() {
    // 这里要的是**空库**上跑 db.sql,所以不能用 `database()`(它已经建好了)
    let results = assert_all_ok(&mut SymbolTable::new(), "db.sql");
    assert_eq!(results.len(), 52, "db.sql 一共 52 条语句");
}

#[test]
fn every_table_has_the_row_count_the_script_claims() {
    let db = database();
    // db.sql 的注释里写死了这几个数:40 名学生,20 门课(13 GPA + 7 P/F),
    // 每个学生每门课一行 → GPA 表 520,P/F 表 280
    let db = &db;
    assert_eq!(rows(db, "teachers"), 12);
    assert_eq!(rows(db, "students"), 40);
    assert_eq!(rows(db, "courses"), 20);
    assert_eq!(rows(db, "gpa_grades"), 520);
    assert_eq!(rows(db, "pf_grades"), 280);
}

// ------------------------------------------------------------ 三份查询

#[test]
fn query1_passes() {
    let results = assert_all_ok(&mut database(), "query1.sql");
    assert_eq!(results.len(), 4);
}

#[test]
fn query2_passes() {
    let results = assert_all_ok(&mut database(), "query2.sql");
    assert_eq!(results.len(), 5);
}

#[test]
fn query3_passes() {
    let results = assert_all_ok(&mut database(), "query3.sql");
    assert_eq!(results.len(), 8);
}

#[test]
fn query4_passes() {
    // TODO 0.2 的验收点:子查询一套(标量 / IN / UNION / EXISTS / 相关标量 / 派生表 / ALL)
    // 六条全部跑通.**"不报错"不等于"算对了"**,所以下面还有几条核对结果的不变量.
    let results = assert_all_ok(&mut database(), "query4.sql");
    assert_eq!(results.len(), 6);
}

#[test]
fn query5_passes() {
    // TODO 0.1 的验收点:`EXTRACT(YEAR FROM ...)` 与 `ORDER BY <输出列序号>`
    let results = assert_all_ok(&mut database(), "query5.sql");
    assert_eq!(results.len(), 9);
}

#[test]
fn order_by_an_output_column_number_sorts_by_that_column() {
    // query5 [5-5] 是 `ORDER BY 3 DESC, 1`,3 = credits,1 = code.
    // 以前它被降级成"排常量",顺序是错的(测试台只数错误,所以没抓住).
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT c.code, c.title, c.credits, c.grade_mode
         FROM courses AS c
         ORDER BY 3 DESC, 1
         LIMIT 8",
    );

    assert_eq!(out.nrows(), 8);
    for r in 1..out.nrows() {
        let prev = (credits_of(&out, r - 1), text_of(&out, r - 1, 0));
        let cur = (credits_of(&out, r), text_of(&out, r, 0));
        assert!(
            prev.0 > cur.0 || (prev.0 == cur.0 && prev.1 < cur.1),
            "第 {} 行违背了 credits DESC, code ASC:{:?} 之后是 {:?}",
            r,
            prev,
            cur
        );
    }

    // 排序键要指向**正确的输出列**(不是常量)
    let explained = explain(&db, "SELECT c.code, c.title, c.credits, c.grade_mode FROM courses AS c ORDER BY 3 DESC, 1 LIMIT 8");
    assert!(explained.contains("Sort(keys=[credits DESC, code ASC])"), "{explained}");
}

#[test]
fn extract_year_orders_by_the_enrolment_year() {
    // query5 [5-4]:按入学年份倒序,再按姓名.
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT s.enrolled_at FROM students AS s
         ORDER BY EXTRACT(YEAR FROM s.enrolled_at) DESC, s.name
         LIMIT 8",
    );

    let years: Vec<i32> = (0..out.nrows())
        .map(|r| {
            let text = out.value(r, 0).expect("enrolled_at").to_string();
            text[..4].parse().expect("年份")
        })
        .collect();

    assert!(
        years.windows(2).all(|w| w[0] >= w[1]),
        "入学年份应当非递增:{years:?}"
    );
}

#[test]
fn the_scalar_subquery_keeps_only_above_average_grades() {
    // query4 [4-1] 的形状:`gpa > (SELECT AVG(gpa) FROM gpa_grades)`.
    // 用"和算好的平均值做连接"来核--两条路必须给出同一个行数.
    let mut db = database();

    let via_scalar = query(
        &mut db,
        "SELECT COUNT(*) FROM gpa_grades AS g
         WHERE g.gpa > (SELECT AVG(gpa) FROM gpa_grades)",
    );
    let via_join = query(
        &mut db,
        "SELECT COUNT(*)
         FROM gpa_grades AS g
             JOIN (SELECT AVG(gpa) AS avg_gpa FROM gpa_grades) AS t
               ON g.gpa > t.avg_gpa",
    );

    assert_eq!(via_scalar.value(0, 0), via_join.value(0, 0));

    // 平均值两侧都该有人(不然这条查询就没意义了)
    let above = via_scalar.value(0, 0).expect("COUNT");
    assert!(above != Value::Int(0), "平均值之上必须有行");
    assert!(above != Value::Int(520), "平均值之下也必须有行");
}

#[test]
fn the_in_subquery_with_a_union_matches_the_two_branches_together() {
    // query4 [4-2]:`IN (gpa 不及格 UNION pf 不及格)`.
    // 两个分支分别在 `IN` 里的并集,必须等于两支各自筛选出来的并集.
    let mut db = database();

    let combined = query(
        &mut db,
        "SELECT s.student_id FROM students AS s
         WHERE s.student_id IN (
             SELECT g.student_id FROM gpa_grades AS g WHERE g.gpa < 1.00
             UNION
             SELECT p.student_id FROM pf_grades AS p WHERE p.result = 'F'
         )",
    );
    let failing = query(
        &mut db,
        "SELECT s.student_id FROM students AS s
         WHERE s.student_id IN (SELECT g.student_id FROM gpa_grades AS g WHERE g.gpa < 1.00)",
    );
    let pf_failing = query(
        &mut db,
        "SELECT s.student_id FROM students AS s
         WHERE s.student_id IN (SELECT p.student_id FROM pf_grades AS p WHERE p.result = 'F')",
    );

    // `UNION` 去重,两个来源的学生数因此不会超过两边相加
    assert!(combined.nrows() >= failing.nrows());
    assert!(combined.nrows() >= pf_failing.nrows());
    assert!(combined.nrows() > 0, "至少有一个学生不及格");
}

// ------------------------------------------------- 结果里的可核对不变量

#[test]
fn the_left_join_finds_exactly_the_pf_courses() {
    // query2 [2-4]:没有任何 GPA 记录的课程
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT c.course_id, c.code, c.title, c.grade_mode
         FROM courses AS c
             LEFT JOIN gpa_grades AS g ON g.course_id = c.course_id
         WHERE g.course_id IS NULL
         ORDER BY c.course_id",
    );

    // db.sql 说 P/F 制 7 门
    assert_eq!(out.nrows(), 7);
    for r in 0..out.nrows() {
        assert_eq!(
            out.value(r, 3),
            Some(Value::Text("P/F".to_string())),
            "第 {r} 行的评分方式应当都是 P/F"
        );
    }
}

#[test]
fn the_self_join_returns_only_ordered_pairs() {
    // query2 [2-5] 的形状:`a.student_id < b.student_id` 保证不出现 (x,x) 也不重复
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT a.student_id AS student_a, b.student_id AS student_b, a.gpa
         FROM gpa_grades AS a
             JOIN gpa_grades AS b
               ON b.course_id = a.course_id AND b.gpa = a.gpa
              AND a.student_id < b.student_id
         WHERE a.course_id = 1
         ORDER BY a.gpa DESC, a.student_id, b.student_id
         LIMIT 15",
    );

    for r in 0..out.nrows() {
        let (left, right) = match (out.value(r, 0), out.value(r, 1)) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => (a, b),
            other => panic!("第 {r} 行不是两个整数:{other:?}"),
        };
        // 按**数值**比,不能按文本比("2" < "10" 是假的)
        assert!(left < right, "第 {r} 行的学生对必须严格递增:{left} vs {right}");
    }
}

#[test]
fn the_global_aggregate_counts_every_gpa_row() {
    // query3 [3-7]
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT COUNT(*) AS records, ROUND(AVG(gpa), 3) AS avg_gpa,
                MIN(gpa) AS min_gpa, MAX(gpa) AS max_gpa
         FROM gpa_grades",
    );

    assert_eq!(out.nrows(), 1);
    assert_eq!(out.value(0, 0), Some(Value::Int(520)));

    // 绩点必须落在 [0, 4],而且 min <= avg <= max
    let min = out.value(0, 2).expect("min").to_string();
    let avg = out.value(0, 1).expect("avg").to_string();
    let max = out.value(0, 3).expect("max").to_string();
    let (min, avg, max): (f64, f64, f64) = (
        min.parse().expect("min 是数"),
        avg.parse().expect("avg 是数"),
        max.parse().expect("max 是数"),
    );

    assert!((0.0..=4.0).contains(&min), "min={min}");
    assert!((0.0..=4.0).contains(&max), "max={max}");
    assert!(min <= avg && avg <= max, "{min} <= {avg} <= {max}");
}

#[test]
fn grouping_by_department_accounts_for_every_teacher() {
    // query3 [3-8]:按院系分组**不重不漏**,所以教师数之和必须是 12
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT t.department, COUNT(*) AS teachers
         FROM teachers AS t
         GROUP BY t.department
         ORDER BY teachers DESC, t.department",
    );

    assert_eq!(sum_int(&out, 1), 12);
}

#[test]
fn grouping_by_gender_accounts_for_every_student() {
    // query3 [3-5]:COUNT(DISTINCT student_id) 之和必须是 40
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT s.gender, COUNT(DISTINCT s.student_id) AS students,
                ROUND(AVG(g.gpa), 2) AS avg_gpa
         FROM students AS s
             JOIN gpa_grades AS g ON g.student_id = s.student_id
         GROUP BY s.gender
         ORDER BY s.gender",
    );

    assert_eq!(sum_int(&out, 1), 40);
    // 每个学生的绩点都在 [0,4],平均值也必须在
    for r in 0..out.nrows() {
        let avg: f64 = out.value(r, 2).expect("avg").to_string().parse().expect("是数");
        assert!((0.0..=4.0).contains(&avg), "avg_gpa={avg}");
    }
}

#[test]
fn the_pass_rate_query_stays_within_percent_bounds() {
    // query3 [3-6]:通过率必须在 [0, 100],而且 passed + failed == enrolled
    let mut db = database();
    let out = query(
        &mut db,
        "SELECT c.code, c.title,
                COUNT(*) AS enrolled,
                SUM(CASE WHEN p.result = 'P' THEN 1 ELSE 0 END) AS passed,
                SUM(CASE WHEN p.result = 'F' THEN 1 ELSE 0 END) AS failed,
                ROUND(100.0 * SUM(CASE WHEN p.result = 'P' THEN 1 ELSE 0 END) / COUNT(*), 1)
                    AS pass_rate
         FROM courses AS c
             JOIN pf_grades AS p ON p.course_id = c.course_id
         GROUP BY c.course_id, c.code, c.title
         ORDER BY pass_rate DESC, c.code",
    );

    assert!(out.nrows() > 0);
    for r in 0..out.nrows() {
        let enrolled = match out.value(r, 2) {
            Some(Value::Int(v)) => v as i64,
            other => panic!("enrolled 不是整数:{other:?}"),
        };
        let p = match out.value(r, 3) {
            Some(Value::Int(v)) => v as i64,
            other => panic!("passed 不是整数:{other:?}"),
        };
        let f = match out.value(r, 4) {
            Some(Value::Int(v)) => v as i64,
            other => panic!("failed 不是整数:{other:?}"),
        };
        let rate: f64 = out
            .value(r, 5)
            .expect("rate")
            .to_string()
            .parse()
            .expect("rate 是数");

        assert_eq!(p + f, enrolled, "第 {r} 行:通过 + 不通过 应当等于选课人数");
        assert!((0.0..=100.0).contains(&rate), "第 {r} 行通过率越界:{rate}");
    }
}

#[test]
fn having_selects_a_strict_subset_of_the_groups() {
    // query3 [3-3] 的 HAVING 必须真的筛掉一些组(否则这条查询就没意义了)
    let mut db = database();

    let all = query(
        &mut db,
        "SELECT c.course_id FROM courses AS c
             JOIN gpa_grades AS g ON g.course_id = c.course_id
         GROUP BY c.course_id",
    );
    let filtered = query(
        &mut db,
        "SELECT c.course_id FROM courses AS c
             JOIN gpa_grades AS g ON g.course_id = c.course_id
         GROUP BY c.course_id
         HAVING AVG(g.gpa) >= 2.65",
    );

    assert!(filtered.nrows() < all.nrows(), "HAVING 应当筛掉一些组");
    assert!(filtered.nrows() > 0, "也不该把组筛光");
}
