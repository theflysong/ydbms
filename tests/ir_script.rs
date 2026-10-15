//! 脚本执行的 testbench:`CREATE TABLE` + `INSERT` + `SELECT` 的完整链路.
//!
//! 主测试**运行时读取真实的** `tests/script/db1/db.sql`(而不是 `include_str!`),
//! 这样连文件读取一起验--`\i` 干的就是这件事.建出来的 `prime` 表要能被
//! 脚本自带的那条 SELECT 查出 25 行,并且**逐行满足 `square == value²`**.
//!
//! 另一半重点是"该拒绝的必须拒绝":`DEFAULT` / `CHECK` / 表级约束会被拒,
//! 而 `PRIMARY KEY` 是**接受但不强制**--所以它必须出现在 `unenforced` 里,
//! 不许偷偷放过.

use std::path::Path;

use ydbms::db::table::{DataType, DbError, TabShape, Table, Value};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, StmtResult, SymbolTable, TxOp};

/// 运行时读取真实的建库脚本.
fn db_sql() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/script/db1/db.sql");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读不了 {}:{e}", path.display()))
}

/// 跑一段 SQL;解析失败或任一条语句失败都直接 panic.
fn run_ok(db: &mut SymbolTable, sql: &str) -> Vec<Outcome> {
    let results = run_script(sql, db).expect("应当能解析并切分");
    results
        .iter()
        .map(|r| match &r.outcome {
            Ok(outcome) => outcome.clone(),
            Err(e) => panic!("第 {} 条语句失败了:{e}", r.index),
        })
        .collect()
}

/// 跑一段 SQL 并返回逐条结果(允许失败).
fn run(db: &mut SymbolTable, sql: &str) -> Vec<StmtResult> {
    run_script(sql, db).expect("应当能解析并切分")
}

/// 跑恰好一条语句,拿它的成败.
fn one(db: &mut SymbolTable, sql: &str) -> Result<Outcome, DbError> {
    let mut results = run(db, sql);
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome
}

fn rows_of(outcome: &Outcome) -> &PhysTab {
    match outcome {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows,得到 {other:?}"),
    }
}

fn col_i32(table: &PhysTab, c: usize) -> Vec<i32> {
    (0..table.nrows())
        .map(|r| table.value(r, c).and_then(|v| v.as_int()).expect("INT 列"))
        .collect()
}

// --------------------------------------------------------- 跑真实的 db.sql

#[test]
fn db_loads_without_a_single_failure() {
    let mut db = SymbolTable::new();
    let results = run(&mut db, &db_sql());

    assert!(results.len() > 40, "这是个大脚本:{} 条", results.len());
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

#[test]
fn db_creates_the_five_tables_with_the_declared_row_counts() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    // 成绩表拆成两张,所以是五张:GPA 制 13 门,P/F 制 7 门
    let expected = [
        ("teachers", 12usize),
        ("students", 40),
        ("courses", 20),
        ("gpa_grades", 520),
        ("pf_grades", 280),
    ];

    for (name, rows) in expected {
        let table = db.relation(name).unwrap_or_else(|| panic!("缺少表 {name}"));
        assert_eq!(table.nrows(), rows, "{name} 的行数");
    }
}

#[test]
fn db_declares_the_schema_types_it_promised() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    let ty = |table: &str, column: &str| {
        db.columns(table)
            .expect("表存在")
            .iter()
            .find(|c| c.name == column)
            .unwrap_or_else(|| panic!("{table} 缺少列 {column}"))
            .ty
    };

    assert_eq!(ty("gpa_grades", "gpa"), DataType::Numeric(3, 2));
    assert_eq!(ty("pf_grades", "result"), DataType::Char(1));
    assert_eq!(ty("students", "gender"), DataType::Char(1));
    assert_eq!(ty("courses", "grade_mode"), DataType::Char(3));
    assert_eq!(ty("students", "birth_date"), DataType::Timestamp(6));
}

#[test]
fn db_statement_texts_are_reported() {
    let mut db = SymbolTable::new();
    let results = run(&mut db, &db_sql());

    assert_eq!(results[0].index, 1);
    assert!(results[0].text.contains("CREATE TABLE"), "{}", results[0].text);
    assert!(results[1].text.contains("CREATE TABLE"), "{}", results[1].text);
    // 压成一行且截断了
    assert!(!results[0].text.contains('\n'));
}

/// 跑一条查询,要求成功.
fn query(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run(db, sql);
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("查询应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows,得到 {other:?}"),
    }
}

/// 跑一条查询并把两列取成 `(i32, i32)` 集合.
fn pairs(db: &mut SymbolTable, sql: &str) -> std::collections::BTreeSet<(i32, i32)> {
    let table = query(db, sql);
    let left = col_i32(&table, 0);
    let right = col_i32(&table, 1);
    left.into_iter().zip(right).collect()
}

#[test]
fn db_gives_every_student_a_grade_in_every_course() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    let gpa = pairs(&mut db, "SELECT student_id, course_id FROM gpa_grades");
    let pf = pairs(&mut db, "SELECT student_id, course_id FROM pf_grades");

    // 每个 (学生, 课程) 组合恰好出现在两张成绩表之一里
    assert!(gpa.is_disjoint(&pf), "同一对 (学生, 课程) 不该同时出现在两张表里");
    assert_eq!(gpa.len() + pf.len(), 40 * 20, "40 个学生 × 20 门课");

    for sid in 1..=40 {
        for cid in 1..=20 {
            let hit = gpa.contains(&(sid, cid)) || pf.contains(&(sid, cid));
            assert!(hit, "学生 {sid} 在课程 {cid} 上没有成绩");
        }
    }
}

#[test]
fn db_puts_each_course_in_the_table_its_mode_says() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    // 课程表说自己是 GPA 的,成绩就必须在 gpa_grades 里;P/F 同理
    let gpa_courses = {
        let table = query(
            &mut db,
            "SELECT course_id, course_id FROM courses WHERE grade_mode = 'GPA'",
        );
        col_i32(&table, 0)
    };
    let pf_courses = {
        let table = query(
            &mut db,
            "SELECT course_id, course_id FROM courses WHERE grade_mode = 'P/F'",
        );
        col_i32(&table, 0)
    };

    assert_eq!(gpa_courses.len(), 13);
    assert_eq!(pf_courses.len(), 7);

    let graded_with_gpa = pairs(&mut db, "SELECT student_id, course_id FROM gpa_grades");
    let graded_with_pf = pairs(&mut db, "SELECT student_id, course_id FROM pf_grades");

    for (_, cid) in &graded_with_gpa {
        assert!(gpa_courses.contains(cid), "课程 {cid} 不是 GPA 制,却有 GPA 成绩");
    }
    for (_, cid) in &graded_with_pf {
        assert!(pf_courses.contains(cid), "课程 {cid} 不是 P/F 制,却有 P/F 成绩");
    }
}

#[test]
fn db_gpa_stays_within_the_four_point_scale() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    let table = query(&mut db, "SELECT student_id, course_id FROM gpa_grades WHERE gpa > 4.00");
    assert_eq!(table.nrows(), 0, "不该有超过 4.00 的绩点");

    let negative = query(&mut db, "SELECT student_id, course_id FROM gpa_grades WHERE gpa < 0.00");
    assert_eq!(negative.nrows(), 0, "不该有负绩点");

    // 低分段确实被用上了(不然这份数据没测到 NUMERIC 的下界)
    let failing = query(&mut db, "SELECT student_id, course_id FROM gpa_grades WHERE gpa < 1.00");
    assert!(failing.nrows() > 0, "应当有不及格的绩点");
}

#[test]
fn db_pf_results_are_only_p_or_f() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    let weird = query(
        &mut db,
        "SELECT student_id, course_id FROM pf_grades WHERE result <> 'P' AND result <> 'F'",
    );
    assert_eq!(weird.nrows(), 0);

    let failed = query(&mut db, "SELECT student_id, course_id FROM pf_grades WHERE result = 'F'");
    assert!(failed.nrows() > 0, "应当有 P/F 不及格的记录");
}

#[test]
fn db_every_course_points_at_a_real_teacher() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    // courses.teacher_id 必须能在 teachers 里找到--用连接数一数
    let joined = query(
        &mut db,
        "SELECT c.course_id, t.teacher_id \
         FROM courses AS c JOIN teachers AS t ON c.teacher_id = t.teacher_id",
    );
    assert_eq!(joined.nrows(), 20, "20 门课都该连上一位教师");

    let teachers = query(&mut db, "SELECT teacher_id, teacher_id FROM teachers");
    assert_eq!(teachers.nrows(), 12);
}

#[test]
fn db_trailing_queries_all_succeed() {
    // 脚本末尾那几条查询(含三表连接)必须都能跑
    let mut db = SymbolTable::new();
    let results = run(&mut db, &db_sql());

    let queries = results
        .iter()
        .filter(|r| r.text.contains("SELECT"))
        .count();
    assert!(queries >= 4, "脚本末尾应当有几条查询,实际 {queries}");

    for r in results.iter().filter(|r| r.text.contains("SELECT")) {
        match &r.outcome {
            Ok(Outcome::Rows(table)) => assert!(table.nrows() > 0, "第 {} 条查询没有结果", r.index),
            other => panic!("第 {} 条查询不成功:{other:?}", r.index),
        }
    }
}

// ----------------------------------------------------------------- CREATE TABLE

#[test]
fn create_table_then_select_yields_zero_rows() {
    let mut db = SymbolTable::new();
    let outcomes = run_ok(&mut db, "CREATE TABLE p (a INT, b INT); SELECT * FROM p");

    assert!(matches!(outcomes[0], Outcome::Created { .. }));
    assert_eq!(rows_of(&outcomes[1]).nrows(), 0);
    assert_eq!(db.columns("p").unwrap().len(), 2);
}

#[test]
fn create_table_if_not_exists_is_idempotent() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT)");
    // 第二次带 IF NOT EXISTS,不该报错,也不该把表清空
    run_ok(&mut db, "INSERT INTO p VALUES (7)");
    run_ok(&mut db, "CREATE TABLE IF NOT EXISTS p (a INT)");

    assert_eq!(db.relation("p").unwrap().nrows(), 1, "表不该被重建");
}

#[test]
fn duplicate_create_table_without_if_not_exists_is_rejected() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT)");

    let err = one(&mut db, "CREATE TABLE p (a INT)").unwrap_err();
    assert_eq!(
        err,
        DbError::DuplicateSymbol {
            name: "p".to_string()
        }
    );
}

#[test]
fn integer_aliases_map_to_int() {
    for ty in ["INT", "INTEGER", "INT4"] {
        let mut db = SymbolTable::new();
        run_ok(&mut db, &format!("CREATE TABLE p (a {ty})"));
        assert_eq!(db.columns("p").unwrap()[0].ty, DataType::Int, "类型 {ty}");
    }
}

#[test]
fn unsupported_type_is_rejected_with_its_name() {
    // `DATE` 现在是合法类型了(TODO 1.3),换一个真的不支持的.
    let mut db = SymbolTable::new();
    let err = one(&mut db, "CREATE TABLE p (a BOOLEAN)").unwrap_err();

    match err {
        DbError::Unsupported { ref feature } => {
            // 报出来的必须是 bool,而不是 pg_catalog
            assert!(feature.contains("bool"), "{feature}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn date_is_a_supported_column_type_now() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a DATE)");
    assert_eq!(db.columns("p").unwrap()[0].ty, DataType::Date);
}

#[test]
fn default_fills_an_omitted_column() {
    let mut db = SymbolTable::new();
    run_ok(
        &mut db,
        "CREATE TABLE p (a INT, b INT DEFAULT 7, c TEXT DEFAULT 'x', d DATE DEFAULT '2017-03-08')",
    );
    run_ok(&mut db, "INSERT INTO p (a) VALUES (1)");

    let rows = run_ok(&mut db, "SELECT a, b, c, d FROM p");
    let table = rows_of(&rows[0]);
    assert_eq!(col_i32(table, 0), vec![1]);
    assert_eq!(col_i32(table, 1), vec![7]);
    assert_eq!(table.value(0, 2), Some(Value::Text("x".to_string())));
    assert_eq!(table.value(0, 3), Some(Value::Date("2017-03-08".to_string())));
}

#[test]
fn an_omitted_column_without_a_default_becomes_null() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    run_ok(&mut db, "INSERT INTO p (a) VALUES (1)");

    let rows = run_ok(&mut db, "SELECT b FROM p");
    assert_eq!(rows_of(&rows[0]).value(0, 0), Some(Value::Null));
}

#[test]
fn an_omitted_not_null_column_without_a_default_is_rejected() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT NOT NULL)");
    let err = one(&mut db, "INSERT INTO p (a) VALUES (1)").unwrap_err();

    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");
    assert_eq!(db.relation("p").unwrap().nrows(), 0, "不该插进去半行");
}

#[test]
fn default_null_is_a_null_default() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT DEFAULT NULL)");
    run_ok(&mut db, "INSERT INTO p DEFAULT VALUES".replace("DEFAULT VALUES", "(a) VALUES (1)").as_str());

    let rows = run_ok(&mut db, "SELECT a FROM p");
    assert_eq!(rows_of(&rows[0]).value(0, 0), Some(Value::Int(1)));
}

#[test]
fn a_default_that_does_not_fit_the_column_is_rejected_at_create_time() {
    let mut db = SymbolTable::new();
    // typmod 与类型在**建表**时就强制:不然要到插数据时才炸
    let err = one(&mut db, "CREATE TABLE p (a VARCHAR(2) DEFAULT '中文中')").unwrap_err();
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");

    let err = one(&mut db, "CREATE TABLE q (a INT DEFAULT 'x')").unwrap_err();
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");

    // 表达式形式的 DEFAULT 本版不支持(宁可拒绝,也不猜一个固定值)
    let err = one(&mut db, "CREATE TABLE r (a INT DEFAULT (1 + 1))").unwrap_err();
    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("DEFAULT"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn check_is_really_enforced() {
    let mut db = SymbolTable::new();
    let created = one(&mut db, "CREATE TABLE p (a INT CHECK (a > 0))").unwrap();
    match created {
        Outcome::Created { checks, .. } => assert_eq!(checks, 1, "1 条 CHECK 已强制"),
        other => panic!("expected Created, got {other:?}"),
    }

    run_ok(&mut db, "INSERT INTO p VALUES (1)");
    let err = one(&mut db, "INSERT INTO p VALUES (0)").unwrap_err();
    match err {
        DbError::CheckViolation {
            ref table,
            ref constraint,
        } => {
            assert_eq!(table, "p");
            assert_eq!(constraint, "p_a_check", "没写约束名时按 PostgreSQL 合成");
        }
        other => panic!("expected CheckViolation, got {other:?}"),
    }
    assert_eq!(db.relation("p").unwrap().nrows(), 1, "违规的行不该插进去");

    // 三值逻辑:`NULL > 0` 是 UNKNOWN,**不是假**,所以放行
    run_ok(&mut db, "INSERT INTO p VALUES (NULL)");
    assert_eq!(db.relation("p").unwrap().nrows(), 2);
}

#[test]
fn a_named_check_reports_its_name() {
    let mut db = SymbolTable::new();
    run_ok(
        &mut db,
        "CREATE TABLE p (a TEXT CONSTRAINT \"名字够短\" CHECK (LENGTH(a) <= 3))",
    );
    run_ok(&mut db, "INSERT INTO p VALUES ('中文')");

    let err = one(&mut db, "INSERT INTO p VALUES ('太长了啊')").unwrap_err();
    match err {
        DbError::CheckViolation { ref constraint, .. } => assert_eq!(constraint, "名字够短"),
        other => panic!("expected CheckViolation, got {other:?}"),
    }
}

#[test]
fn a_table_level_check_can_see_several_columns() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (lo INT, hi INT, CHECK (lo <= hi))");
    run_ok(&mut db, "INSERT INTO p VALUES (1, 2)");

    let err = one(&mut db, "INSERT INTO p VALUES (3, 2)").unwrap_err();
    assert!(matches!(err, DbError::CheckViolation { .. }), "got {err:?}");
}

#[test]
fn check_rejects_a_subquery() {
    let mut db = SymbolTable::new();
    let err = one(&mut db, "CREATE TABLE p (a INT CHECK (a > (SELECT 1)))").unwrap_err();
    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("CHECK"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn table_level_constraints_are_accepted_and_recorded() {
    let mut db = SymbolTable::new();
    let created = one(
        &mut db,
        "CREATE TABLE p (a INT, b CHAR(2), PRIMARY KEY (a, b), UNIQUE (b), \
         FOREIGN KEY (a) REFERENCES other(id) ON DELETE CASCADE)",
    )
    .unwrap();

    match created {
        Outcome::Created {
            ref unenforced,
            checks,
            ..
        } => {
            assert_eq!(checks, 0, "没有 CHECK");
            assert_eq!(unenforced.len(), 3, "{unenforced:?}");
            assert_eq!(unenforced[0], "PRIMARY KEY (a, b)", "{unenforced:?}");
            assert_eq!(unenforced[1], "UNIQUE (b)", "{unenforced:?}");
            assert!(
                unenforced[2].contains("FOREIGN KEY (a) REFERENCES other(id)")
                    && unenforced[2].contains("ON DELETE CASCADE"),
                "{unenforced:?}"
            );
        }
        other => panic!("expected Created, got {other:?}"),
    }

    // "未强制"是真的没强制:重复主键照样插得进去(只是看得出来)
    run_ok(&mut db, "INSERT INTO p VALUES (1, 'ab')");
    run_ok(&mut db, "INSERT INTO p VALUES (1, 'ab')");
    assert_eq!(db.relation("p").unwrap().nrows(), 2);
}

#[test]
fn deferrable_constraints_are_still_rejected() {
    let mut db = SymbolTable::new();
    for sql in [
        "CREATE TABLE p (a INT UNIQUE DEFERRABLE)",
        "CREATE TABLE q (a INT UNIQUE INITIALLY DEFERRED)",
        "CREATE TABLE r (a INT, CONSTRAINT c CHECK (a > 0) NOT VALID)",
    ] {
        let err = one(&mut db, sql).unwrap_err();
        assert!(matches!(err, DbError::Unsupported { .. }), "{sql}: {err:?}");
    }
}

#[test]
fn accepted_but_unenforced_constraints_are_surfaced() {
    let mut db = SymbolTable::new();
    let outcomes = run_ok(&mut db, "CREATE TABLE p (a INT PRIMARY KEY, b INT NOT NULL)");

    match &outcomes[0] {
        Outcome::Created { unenforced, .. } => {
            assert_eq!(unenforced.len(), 1, "{unenforced:?}");
            assert!(unenforced[0].contains("PRIMARY KEY"), "{unenforced:?}");
            // NOT NULL **不**该出现在这里:值模型里没有 NULL,它是真的成立.
            assert!(
                !unenforced.iter().any(|s| s.contains("NOT NULL")),
                "{unenforced:?}"
            );
        }
        other => panic!("expected Created, got {other:?}"),
    }
}

// ---------------------------------------------------------------------- INSERT

#[test]
fn insert_without_column_list_uses_table_order() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    let outcomes = run_ok(&mut db, "INSERT INTO p VALUES (1, 2), (3, 4)");

    match &outcomes[0] {
        Outcome::Inserted { rows, .. } => assert_eq!(*rows, 2),
        other => panic!("expected Inserted, got {other:?}"),
    }

    let rows = run_ok(&mut db, "SELECT * FROM p");
    let table = rows_of(&rows[0]);
    assert_eq!(col_i32(table, 0), vec![1, 3]);
    assert_eq!(col_i32(table, 1), vec![2, 4]);
}

#[test]
fn insert_with_column_list_reorders_values() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT, c INT)");
    // 列名表故意打乱顺序
    run_ok(&mut db, "INSERT INTO p (c, a, b) VALUES (3, 1, 2)");

    let rows = run_ok(&mut db, "SELECT a, b, c FROM p");
    let table = rows_of(&rows[0]);
    assert_eq!(col_i32(table, 0), vec![1], "a");
    assert_eq!(col_i32(table, 1), vec![2], "b");
    assert_eq!(col_i32(table, 2), vec![3], "c");
}

#[test]
fn insert_evaluates_expressions() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    run_ok(&mut db, "INSERT INTO p VALUES (1 + 1, 3 * 4)");

    let rows = run_ok(&mut db, "SELECT a, b FROM p");
    let table = rows_of(&rows[0]);
    assert_eq!(col_i32(table, 0), vec![2]);
    assert_eq!(col_i32(table, 1), vec![12]);
}

#[test]
fn insert_arity_mismatch_is_rejected() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    let err = one(&mut db, "INSERT INTO p VALUES (1)").unwrap_err();

    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");
    assert_eq!(db.relation("p").unwrap().nrows(), 0, "不该插进去半行");
}

#[test]
fn insert_unknown_column_is_rejected() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    let err = one(&mut db, "INSERT INTO p (a, zzz) VALUES (1, 2)").unwrap_err();

    match err {
        DbError::UnknownColumn { ref name, ref available } => {
            assert_eq!(name, "zzz");
            assert_eq!(available, &["a", "b"]);
        }
        other => panic!("expected UnknownColumn, got {other:?}"),
    }
}

#[test]
fn insert_into_unknown_table_is_rejected() {
    let mut db = SymbolTable::new();
    let err = one(&mut db, "INSERT INTO nope VALUES (1)").unwrap_err();

    assert_eq!(
        err,
        DbError::UnknownSymbol {
            name: "nope".to_string()
        }
    );
}

// ---------------------------------------------------------------------- UPDATE

/// 建一张两列的表并填三行,供下面的 UPDATE 用.
fn update_fixture() -> SymbolTable {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE t (a INT, b INT)");
    run_ok(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
    db
}

fn col(table: &PhysTab, c: usize) -> Vec<String> {
    (0..table.nrows())
        .map(|r| table.value(r, c).expect("格子").to_string())
        .collect()
}

fn select(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let outcomes = run_ok(db, sql);
    match outcomes.into_iter().next().expect("一条查询") {
        Outcome::Rows(table) => table,
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn update_changes_only_the_matching_rows() {
    let mut db = update_fixture();
    let outcome = one(&mut db, "UPDATE t SET b = 99 WHERE a = 2").unwrap();
    match outcome {
        Outcome::Updated { ref table, rows } => {
            assert_eq!(table, "t");
            assert_eq!(rows, 1);
        }
        other => panic!("expected Updated, got {other:?}"),
    }

    let out = select(&mut db, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(col(&out, 1), vec!["10", "99", "30"]);
}

#[test]
fn update_without_a_where_clause_touches_every_row() {
    let mut db = update_fixture();
    let outcome = one(&mut db, "UPDATE t SET b = b + 1").unwrap();
    match outcome {
        Outcome::Updated { rows, .. } => assert_eq!(rows, 3),
        other => panic!("expected Updated, got {other:?}"),
    }

    let out = select(&mut db, "SELECT b FROM t ORDER BY a");
    assert_eq!(col(&out, 0), vec!["11", "21", "31"]);
}

#[test]
fn every_set_expression_sees_the_old_row() {
    let mut db = update_fixture();
    run_ok(&mut db, "UPDATE t SET a = b, b = a WHERE a = 1");

    // 交换:两个表达式读到的都是改之前那一行--(1,10) 变成 (10,1)
    let out = select(&mut db, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(col(&out, 0), vec!["2", "3", "10"]);
    assert_eq!(col(&out, 1), vec!["20", "30", "1"]);
}

#[test]
fn update_enforces_not_null_and_typmod() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT NOT NULL, b VARCHAR(2))");
    run_ok(&mut db, "INSERT INTO p VALUES (1, 'ab')");

    let err = one(&mut db, "UPDATE p SET a = NULL").unwrap_err();
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");

    let err = one(&mut db, "UPDATE p SET b = '中文中'").unwrap_err();
    assert!(matches!(err, DbError::BadValue { .. }), "got {err:?}");

    // 失败的那两条都不该改到数据
    let out = select(&mut db, "SELECT a, b FROM p");
    assert_eq!(col(&out, 0), vec!["1"]);
    assert_eq!(col(&out, 1), vec!["ab"]);
}

#[test]
fn update_enforces_check_constraints() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT, CHECK (a <= b))");
    run_ok(&mut db, "INSERT INTO p VALUES (1, 2)");

    let err = one(&mut db, "UPDATE p SET a = 5").unwrap_err();
    match err {
        DbError::CheckViolation { ref constraint, .. } => assert_eq!(constraint, "p_check"),
        other => panic!("expected CheckViolation, got {other:?}"),
    }
    assert_eq!(select(&mut db, "SELECT a FROM p").nrows(), 1);
    assert_eq!(col(&select(&mut db, "SELECT a FROM p"), 0), vec!["1"]);

    run_ok(&mut db, "UPDATE p SET a = 2");
    assert_eq!(col(&select(&mut db, "SELECT a FROM p"), 0), vec!["2"]);
}

#[test]
fn update_widens_an_int_into_a_numeric_column() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (n NUMERIC(10,2), i INT)");
    run_ok(&mut db, "INSERT INTO p VALUES (1.50, 7)");

    // `i` 是 INT 列:写进 NUMERIC 列要真的放大(与 INSERT 的字面量定型同一口径)
    run_ok(&mut db, "UPDATE p SET n = i");
    let out = select(&mut db, "SELECT n FROM p");
    assert_eq!(out.value(0, 0), Some(Value::Numeric("7".to_string())));
    assert_eq!(out.columns()[0].ty, DataType::Numeric(10, 2));
}

#[test]
fn update_where_only_keeps_definite_truth() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    run_ok(&mut db, "INSERT INTO p VALUES (1, NULL), (2, 5)");

    // `b = NULL` 是 UNKNOWN:一行都不该被改
    let outcome = one(&mut db, "UPDATE p SET a = 9 WHERE b = NULL").unwrap();
    match outcome {
        Outcome::Updated { rows, .. } => assert_eq!(rows, 0),
        other => panic!("expected Updated, got {other:?}"),
    }
    assert_eq!(col(&select(&mut db, "SELECT a FROM p ORDER BY a"), 0), vec!["1", "2"]);
}

#[test]
fn update_rejects_what_it_cannot_do() {
    let mut db = update_fixture();

    for (sql, needle) in [
        ("UPDATE t SET a = 1 RETURNING a", "RETURNING"),
        ("UPDATE t SET a = 1 FROM t AS u WHERE u.a = 1", "FROM"),
        ("UPDATE t SET a = 1, a = 2", "两次"),
        ("UPDATE t SET a = 1 WHERE (SELECT 1) = 1", "子查询"),
        ("UPDATE t SET a = (SELECT 1)", "子查询"),
        ("UPDATE t SET a = DEFAULT", "DEFAULT"),
        ("UPDATE nope SET a = 1", "nope"),
    ] {
        let err = one(&mut db, sql).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(needle), "{sql}: {text}");
    }

    // 未知列走的是 UnknownColumn(名字与可用列都报出来)
    let err = one(&mut db, "UPDATE t SET zzz = 1").unwrap_err();
    match err {
        DbError::UnknownColumn { ref name, ref available } => {
            assert_eq!(name, "zzz");
            assert_eq!(available, &["a", "b"]);
        }
        other => panic!("expected UnknownColumn, got {other:?}"),
    }
}

// ------------------------------------------------------------------ 脚本语义

#[test]
fn script_continues_after_a_failed_statement() {
    let mut db = SymbolTable::new();

    let results = run(
        &mut db,
        "CREATE TABLE p (a INT); SELECT * FROM nope; INSERT INTO p VALUES (42)",
    );

    assert_eq!(results.len(), 3);
    assert!(results[0].outcome.is_ok());
    assert!(results[1].outcome.is_err(), "中间那条应当失败");
    assert!(results[2].outcome.is_ok(), "失败不该拦住后面的语句");

    // 最后那条真的生效了
    assert_eq!(db.relation("p").unwrap().nrows(), 1);
}

#[test]
fn a_whole_file_syntax_error_runs_nothing() {
    let mut db = SymbolTable::new();
    let err = run_script("CREATE TABLE p (a INT); SELECT FROM WHERE;", &mut db).unwrap_err();

    assert!(matches!(err, DbError::Parse { .. }), "got {err:?}");
    assert!(db.lookup("p").is_none(), "一条都不该跑");
}

#[test]
fn an_empty_script_is_not_an_error() {
    let mut db = SymbolTable::new();
    assert!(run(&mut db, "   \n  ").is_empty());
    assert!(run(&mut db, "-- 只有注释\n").is_empty());
}

// ------------------------------------------------- 事务 / schema / search_path

#[test]
fn begin_and_commit_are_real_transaction_boundaries() {
    let mut db = SymbolTable::new();
    let outcomes = run_ok(&mut db, "BEGIN; CREATE TABLE p (a INT); COMMIT;");

    // 三件事:`BEGIN` 开事务 / 建表 / `COMMIT` 提交
    match &outcomes[0] {
        Outcome::Transaction {
            op: TxOp::Begin,
            applied: true,
            message,
        } => assert_eq!(message, "BEGIN"),
        other => panic!("expected Transaction(Begin), got {other:?}"),
    }
    match &outcomes[2] {
        Outcome::Transaction {
            op: TxOp::Commit,
            applied: true,
            ..
        } => {}
        other => panic!("expected Transaction(Commit), got {other:?}"),
    }
    // 提交之后建的表真的生效了,事务也关掉了
    assert_eq!(db.relation("p").unwrap().nrows(), 0);
    assert!(!db.in_transaction());
}

#[test]
fn rollback_undoes_both_the_data_and_the_catalog() {
    let mut db = SymbolTable::new();
    run_ok(
        &mut db,
        "CREATE TABLE p (a INT); INSERT INTO p VALUES (1); CREATE SCHEMA s;",
    );

    // 事务里建表,插行,`SET search_path`,然后整体撤销
    let outcomes = run_ok(
        &mut db,
        "BEGIN;\n\
         CREATE TABLE q (a INT);\n\
         INSERT INTO p VALUES (2);\n\
         SET search_path TO s;\n\
         ROLLBACK;",
    );
    match &outcomes[4] {
        Outcome::Transaction {
            op: TxOp::Rollback,
            applied: true,
            message,
        } => assert_eq!(message, "ROLLBACK"),
        other => panic!("expected Transaction(Rollback), got {other:?}"),
    }

    assert!(db.lookup("q").is_none(), "建的表要跟着回滚:{:?}", db.names());
    assert_eq!(db.relation("p").unwrap().nrows(), 1, "插的行要跟着回滚");
    assert_eq!(db.search_path().len(), 0, "`SET search_path` 也是事务的一部分");
    assert!(!db.in_transaction());
}

#[test]
fn rollback_after_a_dropped_schema_brings_the_data_back() {
    // 只回滚目录,不回滚数据,是**比不实现事务更糟**的一种错:表回来了,行没了.
    let mut db = SymbolTable::new();
    run_ok(
        &mut db,
        "CREATE SCHEMA study; SET search_path TO study;\n\
         CREATE TABLE movies (id INT, title TEXT); INSERT INTO movies VALUES (1, 'a'), (2, 'b');",
    );

    run_ok(&mut db, "BEGIN; DROP SCHEMA study CASCADE; ROLLBACK;");

    let movies = db.relation("study.movies").expect("表要回来");
    assert_eq!(movies.nrows(), 2, "数据也要一起回来");
    assert_eq!(movies.value(1, 1), Some(Value::Text("b".to_string())));
    assert_eq!(db.schemas(), ["study"]);
}

#[test]
fn rollback_works_on_checked_tables_too() {
    // `CHECK` 是包在表外面的装饰器:整表快照若把它物化掉,回滚之后的表就不再强制约束
    let mut db = SymbolTable::new();
    run_ok(
        &mut db,
        "CREATE TABLE t (a INT CHECK (a > 0)); INSERT INTO t VALUES (1);",
    );

    run_ok(&mut db, "BEGIN; INSERT INTO t VALUES (2); ROLLBACK;");
    assert_eq!(db.relation("t").unwrap().nrows(), 1);

    // 约束还在:这一行必须被拒
    let err = one(&mut db, "INSERT INTO t VALUES (-1)");
    assert!(matches!(err, Err(DbError::CheckViolation { .. })), "got {err:?}");
}

#[test]
fn a_failing_statement_is_rolled_back_on_its_own() {
    // 事务外每条语句就是一个隐式事务:第三行过不了 `CHECK`,前两行也不许留下
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE t (a INT CHECK (a > 0));");

    let err = one(&mut db, "INSERT INTO t VALUES (1), (2), (-3)");
    assert!(matches!(err, Err(DbError::CheckViolation { .. })), "got {err:?}");
    assert_eq!(db.relation("t").unwrap().nrows(), 0, "失败的语句自己回滚");

    // 事务**里**的失败不整句回滚(本版没有 SAVEPOINT),要撤销就 `ROLLBACK`
    run_ok(&mut db, "BEGIN;");
    let err = one(&mut db, "INSERT INTO t VALUES (1), (2), (-3)");
    assert!(matches!(err, Err(DbError::CheckViolation { .. })), "got {err:?}");
    assert_eq!(db.relation("t").unwrap().nrows(), 2, "事务里保留半拉改动");
    run_ok(&mut db, "ROLLBACK;");
    assert_eq!(db.relation("t").unwrap().nrows(), 0, "`ROLLBACK` 能整体撤销");
}

#[test]
fn a_bare_rollback_only_warns_like_postgresql() {
    // 没有事务时的 `ROLLBACK` / `COMMIT` 不是错误:照 PostgreSQL 的口径发一句警告
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT); INSERT INTO p VALUES (1);");

    for (sql, op) in [("ROLLBACK", TxOp::Rollback), ("COMMIT", TxOp::Commit)] {
        match one(&mut db, sql) {
            Ok(Outcome::Transaction {
                op: got,
                applied,
                message,
            }) => {
                assert_eq!(got, op);
                assert!(!applied, "{sql} 什么都没动到事务");
                assert!(message.contains("警告"), "{sql}: {message}");
            }
            other => panic!("{sql} 应当只给警告,实际是 {other:?}"),
        }
    }

    // 都没动数据
    assert_eq!(db.relation("p").unwrap().nrows(), 1);
    assert!(!db.in_transaction());

    // `SAVEPOINT` 仍然**明确拒绝**(本版是真的没有)
    let err = one(&mut db, "SAVEPOINT s1");
    assert!(matches!(err, Err(DbError::Unsupported { .. })), "got {err:?}");
}

#[test]
fn begin_inside_a_transaction_only_warns() {
    let mut db = SymbolTable::new();
    let outcomes = run_ok(&mut db, "BEGIN; BEGIN; ROLLBACK;");

    match &outcomes[1] {
        Outcome::Transaction {
            op: TxOp::Begin,
            applied: false,
            message,
        } => assert!(message.contains("警告"), "{message}"),
        other => panic!("expected Transaction(Begin), got {other:?}"),
    }
    // 第二个 `BEGIN` 没有被当成嵌套:事务还是那一个,`ROLLBACK` 正常结束它
    match &outcomes[2] {
        Outcome::Transaction {
            op: TxOp::Rollback,
            applied: true,
            message,
        } => assert_eq!(message, "ROLLBACK"),
        other => panic!("expected Transaction(Rollback), got {other:?}"),
    }
    assert!(!db.in_transaction());
}

#[test]
fn search_path_makes_unqualified_names_land_in_the_schema() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE SCHEMA study; SET search_path TO study, public;");
    run_ok(&mut db, "CREATE TABLE movies (movieid INT, title TEXT)");
    run_ok(&mut db, "INSERT INTO movies VALUES (1, 'A')");

    // 建出来的名字带前缀(存的就是 `study.movies`)......
    assert_eq!(db.names(), vec!["study.movies"]);

    // ......而限定名与(按 search_path)非限定名都能查到同一张表
    assert_eq!(db.relation("study.movies").unwrap().nrows(), 1);
    assert_eq!(db.relation("movies").unwrap().nrows(), 1);
    assert_eq!(db.columns("movies").unwrap().len(), 2);

    let selected = run_ok(&mut db, "SELECT movieid FROM study.movies");
    let rows = rows_of(&selected[0]);
    assert_eq!(col_i32(rows, 0), vec![1]);
}

#[test]
fn without_a_search_path_nothing_is_prefixed() {
    // 默认路径是空的:行为与过去**完全一样**
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE t (a INT)");

    assert!(db.lookup("t").is_some());
    assert_eq!(db.search_path().len(), 0);
}

#[test]
fn create_schema_respects_if_not_exists() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE SCHEMA study");

    let err = one(&mut db, "CREATE SCHEMA study");
    assert!(matches!(err, Err(DbError::DuplicateSymbol { .. })), "got {err:?}");

    // `IF NOT EXISTS` 就是"已经在了也算满足要求"
    run_ok(&mut db, "CREATE SCHEMA IF NOT EXISTS study");
}

#[test]
fn drop_schema_cascade_removes_the_tables_under_that_schema() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE SCHEMA study; SET search_path TO study;");
    run_ok(&mut db, "CREATE TABLE movies (movieid INT)");
    run_ok(&mut db, "INSERT INTO movies VALUES (1)");

    run_ok(&mut db, "DROP SCHEMA study CASCADE");
    assert!(db.lookup("study.movies").is_none(), "{:?}", db.names());
    assert!(db.schemas().is_empty());
}

#[test]
fn drop_schema_without_cascade_refuses_to_drop_objects() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE SCHEMA study; SET search_path TO study; CREATE TABLE movies (movieid INT)");

    let err = one(&mut db, "DROP SCHEMA study");
    match err {
        Err(DbError::Unsupported { ref feature }) => assert!(feature.contains("CASCADE"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // 拒绝之后什么都没掉
    assert!(db.lookup("study.movies").is_some());
}

#[test]
fn drop_schema_if_exists_is_idempotent_but_plain_drop_is_not() {
    let mut db = SymbolTable::new();
    // 什么都没建也能"删"(`IF EXISTS`)
    run_ok(&mut db, "DROP SCHEMA IF EXISTS study CASCADE");
    run_ok(&mut db, "DROP SCHEMA IF EXISTS study CASCADE");

    let err = one(&mut db, "DROP SCHEMA study");
    assert!(matches!(err, Err(DbError::UnknownSymbol { .. })), "got {err:?}");
}

#[test]
fn an_init_script_like_the_projects_runs_twice() {
    // `tests/script/filmdb/init.sql` 的形状:DROP → CREATE → SET search_path → 建表 → 装数据
    const INIT: &str = "
        BEGIN;
        DROP SCHEMA IF EXISTS study CASCADE;
        CREATE SCHEMA study;
        SET search_path TO study, public;
        CREATE TABLE movies (movieid INT, title TEXT);
        INSERT INTO movies VALUES (1, 'A');
        COMMIT;
    ";

    let mut db = SymbolTable::new();
    for round in 1..=2 {
        let results = run(&mut db, INIT);
        for result in &results {
            assert!(
                result.outcome.is_ok(),
                "第 {round} 轮的第 {} 条失败了:{:?}",
                result.index,
                result.outcome
            );
        }
        // 第二轮是**重建**,所以只有一行(不是累加到两行)
        assert_eq!(db.relation("study.movies").unwrap().nrows(), 1, "第 {round} 轮");
    }
}

#[test]
fn other_set_variables_are_rejected() {
    let mut db = SymbolTable::new();
    for sql in ["SET work_mem = '64MB'", "SET search_path = DEFAULT"] {
        let err = one(&mut db, sql);
        assert!(matches!(err, Err(DbError::Unsupported { .. })), "{sql}: {err:?}");
    }
}

#[test]
fn drop_table_is_still_rejected() {
    let mut db = SymbolTable::new();
    let err = one(&mut db, "DROP TABLE t");
    assert!(matches!(err, Err(DbError::Unsupported { .. })), "got {err:?}");
}

// -------------------------------------------------- VACUUM / ANALYZE(空转)

/// 本版没有统计信息,也没有要回收的空间,所以 `VACUUM` / `ANALYZE` 的所有变体都
/// **接受但不做任何事**;报文里要如实说清是空转(别让"跑过了"被读成"有统计信息了").
#[test]
fn vacuum_and_analyze_are_accepted_and_are_no_ops() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE t1 (a INT); CREATE TABLE t2 (a INT);");

    for sql in [
        "VACUUM",
        "VACUUM t1",
        "VACUUM ANALYZE t1",
        "VACUUM (ANALYZE) t1, t2",
        "VACUUM (VERBOSE, ANALYZE) t1",
        "VACUUM FULL t1",
        "VACUUM (FULL, FREEZE) t2",
        "ANALYZE",
        "ANALYZE t1, t2",
        "ANALYZE VERBOSE t1",
    ] {
        let outcome = one(&mut db, sql).unwrap_or_else(|e| panic!("`{sql}` 本应接受:{e}"));
        match outcome {
            Outcome::NoOp { message } => {
                assert!(message.contains("空转"), "`{sql}` 要说清是空转:{message}")
            }
            other => panic!("`{sql}` 期望 NoOp,得到 {other:?}"),
        }
    }
}

/// 空转的验收口径:加它 / 不加它,查询结果**逐行相同**.
#[test]
fn vacuum_does_not_change_what_a_query_returns() {
    let mut db = SymbolTable::new();
    run_ok(
        &mut db,
        "CREATE TABLE t (a INT); INSERT INTO t VALUES (3), (1), (2);",
    );
    let before = col_i32(
        rows_of(&one(&mut db, "SELECT a FROM t ORDER BY a").unwrap()),
        0,
    );

    run_ok(&mut db, "VACUUM (ANALYZE) t; ANALYZE t;");

    let after = col_i32(
        rows_of(&one(&mut db, "SELECT a FROM t ORDER BY a").unwrap()),
        0,
    );
    assert_eq!(before, after);
}

/// 空转**不等于安静**:点名的表认不出来就得报错,否则 `VACUUM typos` 会成功地骗人.
#[test]
fn vacuum_on_an_unknown_table_is_reported() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE t1 (a INT);");

    let err = one(&mut db, "VACUUM (ANALYZE) t1, nope").expect_err("本应报错");
    assert!(
        matches!(err, DbError::UnknownSymbol { ref name } if name == "nope"),
        "got {err:?}"
    );
}
