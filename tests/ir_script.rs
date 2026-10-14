//! 脚本执行的 testbench：`CREATE TABLE` + `INSERT` + `SELECT` 的完整链路。
//!
//! 主测试**运行时读取真实的** `tests/script/db1/db.sql`（而不是 `include_str!`），
//! 这样连文件读取一起验——`\i` 干的就是这件事。建出来的 `prime` 表要能被
//! 脚本自带的那条 SELECT 查出 25 行，并且**逐行满足 `square == value²`**。
//!
//! 另一半重点是"该拒绝的必须拒绝"：`DEFAULT` / `CHECK` / 表级约束会被拒，
//! 而 `PRIMARY KEY` 是**接受但不强制**——所以它必须出现在 `unenforced` 里，
//! 不许偷偷放过。

use std::path::Path;

use ydbms::db::table::{DataType, DbError, Table};
use ydbms::db::PhysTab;
use ydbms::ir::{run_script, Outcome, StmtResult, SymbolTable};

/// 运行时读取真实的建库脚本。
fn db_sql() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/script/db1/db.sql");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读不了 {}：{e}", path.display()))
}

/// 跑一段 SQL；解析失败或任一条语句失败都直接 panic。
fn run_ok(db: &mut SymbolTable, sql: &str) -> Vec<Outcome> {
    let results = run_script(sql, db).expect("应当能解析并切分");
    results
        .iter()
        .map(|r| match &r.outcome {
            Ok(outcome) => outcome.clone(),
            Err(e) => panic!("第 {} 条语句失败了：{e}", r.index),
        })
        .collect()
}

/// 跑一段 SQL 并返回逐条结果（允许失败）。
fn run(db: &mut SymbolTable, sql: &str) -> Vec<StmtResult> {
    run_script(sql, db).expect("应当能解析并切分")
}

/// 跑恰好一条语句，拿它的成败。
fn one(db: &mut SymbolTable, sql: &str) -> Result<Outcome, DbError> {
    let mut results = run(db, sql);
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome
}

fn rows_of(outcome: &Outcome) -> &PhysTab {
    match outcome {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows，得到 {other:?}"),
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

    assert!(results.len() > 40, "这是个大脚本：{} 条", results.len());
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了：{:?}", r.index, r.outcome);
    }
}

#[test]
fn db_creates_the_five_tables_with_the_declared_row_counts() {
    let mut db = SymbolTable::new();
    run(&mut db, &db_sql());

    // 成绩表拆成两张，所以是五张：GPA 制 13 门、P/F 制 7 门
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

/// 跑一条查询，要求成功。
fn query(db: &mut SymbolTable, sql: &str) -> PhysTab {
    let mut results = run(db, sql);
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    match results.remove(0).outcome.expect("查询应当成功") {
        Outcome::Rows(table) => table,
        other => panic!("期望 Rows，得到 {other:?}"),
    }
}

/// 跑一条查询并把两列取成 `(i32, i32)` 集合。
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

    // 课程表说自己是 GPA 的，成绩就必须在 gpa_grades 里；P/F 同理
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
        assert!(gpa_courses.contains(cid), "课程 {cid} 不是 GPA 制，却有 GPA 成绩");
    }
    for (_, cid) in &graded_with_pf {
        assert!(pf_courses.contains(cid), "课程 {cid} 不是 P/F 制，却有 P/F 成绩");
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

    // 低分段确实被用上了（不然这份数据没测到 NUMERIC 的下界）
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

    // courses.teacher_id 必须能在 teachers 里找到——用连接数一数
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
    // 脚本末尾那几条查询（含三表连接）必须都能跑
    let mut db = SymbolTable::new();
    let results = run(&mut db, &db_sql());

    let queries = results
        .iter()
        .filter(|r| r.text.contains("SELECT"))
        .count();
    assert!(queries >= 4, "脚本末尾应当有几条查询，实际 {queries}");

    for r in results.iter().filter(|r| r.text.contains("SELECT")) {
        match &r.outcome {
            Ok(Outcome::Rows(table)) => assert!(table.nrows() > 0, "第 {} 条查询没有结果", r.index),
            other => panic!("第 {} 条查询不成功：{other:?}", r.index),
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
    // 第二次带 IF NOT EXISTS，不该报错，也不该把表清空
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
    // `TEXT` 现在是合法类型了，换一个真的不支持的。
    let mut db = SymbolTable::new();
    let err = one(&mut db, "CREATE TABLE p (a DATE)").unwrap_err();

    match err {
        DbError::Unsupported { ref feature } => {
            // 报出来的必须是 date，而不是 pg_catalog
            assert!(feature.contains("date"), "{feature}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn default_constraint_is_rejected() {
    let mut db = SymbolTable::new();
    let err = one(&mut db, "CREATE TABLE p (a INT DEFAULT 1)").unwrap_err();

    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("DEFAULT"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn check_constraint_is_rejected() {
    let mut db = SymbolTable::new();
    let err = one(&mut db, "CREATE TABLE p (a INT CHECK (a > 0))").unwrap_err();

    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("CHECK"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn table_level_constraint_is_rejected() {
    let mut db = SymbolTable::new();
    let err = one(&mut db, "CREATE TABLE p (a INT, PRIMARY KEY (a))").unwrap_err();

    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("表级约束"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
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
            // NOT NULL **不**该出现在这里：值模型里没有 NULL，它是真的成立。
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
fn insert_omitting_a_column_is_rejected() {
    let mut db = SymbolTable::new();
    run_ok(&mut db, "CREATE TABLE p (a INT, b INT)");
    let err = one(&mut db, "INSERT INTO p (a) VALUES (1)").unwrap_err();

    match err {
        DbError::Unsupported { ref feature } => assert!(feature.contains("省略"), "{feature}"),
        other => panic!("expected Unsupported, got {other:?}"),
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
