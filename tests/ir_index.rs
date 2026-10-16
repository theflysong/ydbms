//! TODO 3.6 的 testbench:索引是**目录对象**(`CREATE INDEX` / `DROP INDEX`),
//! 外加 TODO 8.7 那一半:`PRIMARY KEY` / 单列 `UNIQUE` **自动**建索引,主键那份
//! **真的强制唯一性**.
//!
//! 四件事要盯住:
//!
//! 1. **只建一次**:`CREATE INDEX` 扫一遍那一列把索引建出来,之后每条查询都用它,
//!    不再重建(用一张会数数的表看得见);
//! 2. **写得对**:`INSERT` 把新键追加进去(不重建),`UPDATE` 之后索引失效,
//!    下次用时只重建一次--**绝不能**拿旧行号回答新键(那是安静的错答案);
//! 3. **该拒绝的拒绝**:部分索引,多列/表达式键,`CONCURRENTLY` 一律显式报错;
//!    `IF NOT EXISTS` / `IF EXISTS` 幂等;
//! 4. **主键那份强制唯一性**(8.7):重复主键在三条写路径上都报错,`INSERT` 失败
//!    之后表里一行都没多;而 `UNIQUE` 列上的索引**只当访问路径**,重复键照样放行.

use std::cell::Cell;
use std::rc::Rc;

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Result as DbResult, Table, TabShape, Value};
use ydbms::ir::{compile, execute, run_script, Outcome, SymbolTable};

/// 分开数 `row()`(整行)与 `value()`(单格)的次数--建索引只该读**一列**.
#[derive(Default)]
struct Counts {
    rows: Cell<usize>,
    cells: Cell<usize>,
}

/// 一张会数数的 `t(x INT)`.
struct CountingTable {
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    counts: Rc<Counts>,
}

impl CountingTable {
    fn new(tag: &str, values: &[i32], counts: &Rc<Counts>) -> Self {
        let shape = ColumnsShape::new(vec![Column::new(tag, DataType::Int)]);
        CountingTable {
            shape,
            rows: values.iter().map(|v| vec![Value::Int(*v)]).collect(),
            counts: Rc::clone(counts),
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
        self.counts.rows.set(self.counts.rows.get() + 1);
        self.rows.get(r).cloned()
    }

    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.counts.cells.set(self.counts.cells.get() + 1);
        self.rows.get(r).and_then(|row| row.get(c)).cloned()
    }

    fn insert(&mut self, row: Vec<Value>) -> DbResult<usize> {
        self.rows.push(row);
        Ok(self.rows.len() - 1)
    }

    fn update_row(&mut self, r: usize, row: Vec<Value>) -> DbResult<()> {
        let nrows = self.rows.len();
        if r >= nrows {
            return Err(DbError::RowOutOfBounds { index: r, nrows });
        }
        self.rows[r] = row;
        Ok(())
    }
}

/// 装着 `t(x INT)` 的库,外加它的计数器.
fn counting(values: &[i32]) -> (SymbolTable, Rc<Counts>) {
    let counts = Rc::new(Counts::default());
    let mut symbols = SymbolTable::new();
    symbols
        .define("t", CountingTable::new("x", values, &counts))
        .unwrap();
    (symbols, counts)
}

/// 跑一段 SQL,要求全部成功.
fn ok(db: &mut SymbolTable, sql: &str) {
    let results = run_script(sql, db).expect("应当能解析");
    for r in &results {
        assert!(r.outcome.is_ok(), "第 {} 条失败了:{:?}", r.index, r.outcome);
    }
}

/// 跑一条语句并要求失败.
fn failure(db: &mut SymbolTable, sql: &str) -> DbError {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results.remove(0).outcome.expect_err("本应失败")
}

/// 一条查询的行数.
fn rows(db: &SymbolTable, sql: &str) -> usize {
    let plan = compile(sql, db).unwrap_or_else(|e| panic!("降级失败:{e}"));
    execute(&plan, db).unwrap_or_else(|e| panic!("执行失败:{e}")).nrows()
}

/// 计划文本.
fn explain(db: &SymbolTable, sql: &str) -> String {
    compile(sql, db)
        .and_then(|plan| plan.explain(db))
        .unwrap_or_else(|e| panic!("降级失败:{e}"))
}

// -------------------------------------------------------------- 只建一次

#[test]
fn an_index_is_built_once_and_reused_by_later_queries() {
    let (mut db, counts) = counting(&[1, 2, 3, 4, 5]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    // `CREATE INDEX` 当场扫一遍这一列
    assert_eq!(counts.cells.get(), 5, "CREATE INDEX 读一列");
    let after_create = counts.cells.get();

    // 连着两条查询:都走索引,都**不**重建
    for round in 1..=2 {
        assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 3"), 1, "第 {round} 条");
        assert_eq!(
            counts.cells.get(),
            after_create,
            "第 {round} 条不该重新建索引"
        );
    }
    // 只有命中的那两行被整行取出
    assert_eq!(counts.rows.get(), 2);
}

#[test]
fn explain_only_shows_an_index_path_when_the_catalogue_has_one() {
    let (mut db, _) = counting(&[1, 2, 3]);

    // 没建索引:顺序扫描(本版不为"也许有用"先扫一遍表)
    assert_eq!(explain(&db, "SELECT x FROM t WHERE x = 2"), "Scan(t, filter=[x = 2])\n");

    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    assert_eq!(
        explain(&db, "SELECT x FROM t WHERE x = 2"),
        "Scan(t, index=[x], filter=[x = 2])\n"
    );
}

// ---------------------------------------------------------------- 写得对

#[test]
fn insert_maintains_the_index_without_a_rescan() {
    let (mut db, counts) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    let after_create = counts.cells.get();

    ok(&mut db, "INSERT INTO t VALUES (4)");
    assert_eq!(counts.cells.get(), after_create, "维护索引不扫表");

    // 新行查得到,而且索引仍然有效(不再重建)
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 4"), 1);
    assert_eq!(counts.cells.get(), after_create);
}

#[test]
fn update_invalidates_the_index_and_it_is_rebuilt_once() {
    let (mut db, counts) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    ok(&mut db, "UPDATE t SET x = 30 WHERE x = 3");
    let after_update = counts.cells.get();

    // **绝不能用旧行号回答新键**:旧键查不到,新键查得到
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 3"), 0, "旧键不该命中");
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 30"), 1, "新键要命中");
    assert_eq!(
        counts.cells.get(),
        after_update + 3,
        "第一次用到时重建一遍(只读一列 3 格)"
    );

    // 第二条查询不再重建
    let rebuilt = counts.cells.get();
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 30"), 1);
    assert_eq!(counts.cells.get(), rebuilt);
}

#[test]
fn dropping_an_index_takes_the_access_path_away() {
    let (mut db, counts) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    let after_create = counts.cells.get();

    ok(&mut db, "DROP INDEX t_x");
    assert!(!explain(&db, "SELECT x FROM t WHERE x = 2").contains("index="));

    // 结果不变,但现在是顺序扫描(不再读那一列去建索引)
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x = 2"), 1);
    assert_eq!(counts.cells.get(), after_create, "没有索引就不该再读那一列");
}

// -------------------------------------------------------------- 目录的账

#[test]
fn create_index_is_recorded_in_the_catalogue() {
    let (mut db, _) = counting(&[1, 2, 3]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    assert_eq!(
        db.indexes(),
        vec![("t_x".to_string(), "t".to_string(), 0)],
        "索引名,表名,列都要记下来"
    );

    // 名字与表/别名在**同一个名单**里:重名报错(PostgreSQL 也是这么定的)
    let err = failure(&mut db, "CREATE INDEX t ON t (x)");
    assert!(matches!(err, DbError::DuplicateSymbol { .. }), "got {err:?}");

    // `IF NOT EXISTS` 命中时视为满足要求
    ok(&mut db, "CREATE INDEX IF NOT EXISTS t_x ON t (x)");
    assert_eq!(db.indexes().len(), 1);

    // 但表名/列名仍然要先解出来才谈得上"命中"(与 PostgreSQL 的一处记录在案的差异:
    // PG 命中名字之后连表列都不看,我们宁可把错字报出来)
    let err = failure(&mut db, "CREATE INDEX IF NOT EXISTS t_x ON t (zzz)");
    assert!(matches!(err, DbError::UnknownColumn { .. }), "got {err:?}");

    // 表 / 列必须存在
    let err = failure(&mut db, "CREATE INDEX i ON nope (x)");
    assert!(matches!(err, DbError::UnknownSymbol { .. }), "got {err:?}");
    let err = failure(&mut db, "CREATE INDEX i ON t (zzz)");
    assert!(matches!(err, DbError::UnknownColumn { .. }), "got {err:?}");
}

#[test]
fn drop_index_if_exists_is_idempotent() {
    let (mut db, _) = counting(&[1]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");

    let err = failure(&mut db, "DROP INDEX nope");
    assert!(matches!(err, DbError::UnknownSymbol { .. }), "got {err:?}");

    ok(&mut db, "DROP INDEX t_x");
    assert!(db.indexes().is_empty());
    ok(&mut db, "DROP INDEX IF EXISTS t_x");
}

#[test]
fn create_index_rejects_what_it_cannot_do() {
    let (mut db, _) = counting(&[1]);
    ok(&mut db, "CREATE TABLE c (a INT, b INT)");

    for (sql, needle) in [
        ("CREATE INDEX CONCURRENTLY i ON t (x)", "CONCURRENTLY"),
        ("CREATE INDEX i ON t (x) WHERE x > 1", "部分索引"),
        ("CREATE INDEX i ON c (a, b)", "索引键的个数"),
        ("CREATE INDEX i ON c ((a + b))", "裸列"),
        ("CREATE INDEX i ON c USING hash (a)", "USING"),
        ("CREATE INDEX i ON c (a) INCLUDE (b)", "INCLUDE"),
    ] {
        let err = failure(&mut db, sql);
        let text = err.to_string();
        assert!(text.contains(needle), "{sql}: {text}");
        assert!(db.indexes().is_empty(), "{sql}: 不该留下索引");
    }
}

#[test]
fn drop_schema_cascade_also_drops_the_indexes() {
    let (mut db, _) = counting(&[1]);
    ok(
        &mut db,
        "CREATE SCHEMA study; SET search_path TO study; CREATE TABLE m (a INT); \
         CREATE INDEX m_a ON m (a)",
    );
    // 索引的生效名跟着 `search_path` 走,与建表同一套规则
    assert_eq!(db.indexes(), vec![("study.m_a".to_string(), "study.m".to_string(), 0)]);

    // 不写 CASCADE 时如实报出"名下还有东西"
    let err = failure(&mut db, "DROP SCHEMA study");
    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");

    ok(&mut db, "DROP SCHEMA study CASCADE");
    assert!(db.indexes().is_empty(), "schema 下的索引也要清掉");
}

// ---------------------------------------------------------- 语句层的输出

#[test]
fn the_create_index_message_reports_the_column_and_the_keys() {
    let (mut db, _) = counting(&[1, 2, 3]);
    let results = run_script("CREATE INDEX t_x ON t (x)", &mut db).unwrap();

    match &results[0].outcome {
        Ok(Outcome::NoOp { message }) => {
            assert!(message.contains("t(x)"), "{message}");
            assert!(message.contains("3 个不同的键"), "{message}");
        }
        other => panic!("expected NoOp, got {other:?}"),
    }
}

// ------------------------------------------- 表约束自动建索引,主键强制唯一性(8.7)

/// 一个空库,外加一张 `p(k INT, v INT)`.
fn fresh() -> SymbolTable {
    SymbolTable::new()
}

/// `CREATE TABLE` 那条语句的结果.
fn created(db: &mut SymbolTable, sql: &str) -> Outcome {
    let mut results = run_script(sql, db).expect("应当能解析");
    assert_eq!(results.len(), 1, "期望恰好一条语句");
    results
        .remove(0)
        .outcome
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

#[test]
fn a_primary_key_column_gets_an_index_named_like_postgresql() {
    let mut db = fresh();
    let outcome = created(&mut db, "CREATE TABLE p (k INT PRIMARY KEY, v INT)");

    // 报文里如实说出"顺手建了哪一条索引"(名字 + 列名)
    match &outcome {
        Outcome::Created { indexes, .. } => {
            assert_eq!(indexes.len(), 1, "{indexes:?}");
            assert_eq!(indexes[0].name, "p_pkey", "{indexes:?}");
            assert_eq!(indexes[0].column, "k", "{indexes:?}");
        }
        other => panic!("expected Created, got {other:?}"),
    }

    // 目录里真的有它,而且**立刻**就建好了(建表时表是空的,代价为零)
    assert_eq!(
        db.indexes(),
        vec![("p_pkey".to_string(), "p".to_string(), 0)],
        "主键的索引要进目录"
    );
    // 访问路径当场就是它(`v` 要投影,所以 `Scan` 上面还有一个 `Project`)
    assert!(
        explain(&db, "SELECT v FROM p WHERE k = 1").contains("Scan(p, index=[k], filter=[k = 1])"),
        "{}",
        explain(&db, "SELECT v FROM p WHERE k = 1")
    );

    // 表级写法与列级写法是**同一条路**
    let mut db = fresh();
    created(&mut db, "CREATE TABLE q (k INT, v INT, PRIMARY KEY (k))");
    assert_eq!(db.indexes(), vec![("q_pkey".to_string(), "q".to_string(), 0)]);

    // 列级 `UNIQUE` 的名字照 PostgreSQL 是 `<表>_<列>_key`
    let mut db = fresh();
    created(&mut db, "CREATE TABLE r (k INT UNIQUE, v INT)");
    assert_eq!(db.indexes(), vec![("r_k_key".to_string(), "r".to_string(), 0)]);

    // 表级单列 `UNIQUE` 也是
    let mut db = fresh();
    created(&mut db, "CREATE TABLE s (k INT, UNIQUE (k))");
    assert_eq!(db.indexes(), vec![("s_k_key".to_string(), "s".to_string(), 0)]);
}

#[test]
fn a_primary_key_rejects_duplicate_keys() {
    let mut db = fresh();
    ok(&mut db, "CREATE TABLE p (k INT PRIMARY KEY, v INT)");
    ok(&mut db, "INSERT INTO p VALUES (1, 10)");

    // 第二条撞了:报错,而且**表里一行都没多**(判在行落库之前)
    let err = failure(&mut db, "INSERT INTO p VALUES (1, 20)");
    assert!(matches!(err, DbError::DuplicateKey { .. }), "got {err:?}");
    assert!(err.to_string().contains("p_pkey"), "{err}");
    assert_eq!(rows(&db, "SELECT v FROM p"), 1, "失败的 INSERT 不该留下行");

    // 多行 `VALUES` 里撞在第二行:整句回滚(语句级原子性),一行都不留
    let err = failure(&mut db, "INSERT INTO p VALUES (2, 20), (1, 30)");
    assert!(matches!(err, DbError::DuplicateKey { .. }), "got {err:?}");
    assert_eq!(rows(&db, "SELECT v FROM p"), 1, "整句回滚");

    // 不撞的那些照旧进得去,而且索引仍然有效(不重建)
    ok(&mut db, "INSERT INTO p VALUES (2, 20)");
    assert_eq!(rows(&db, "SELECT v FROM p WHERE k = 2"), 1);
}

#[test]
fn an_update_that_collides_on_the_primary_key_is_refused() {
    let mut db = fresh();
    ok(&mut db, "CREATE TABLE p (k INT PRIMARY KEY, v INT)");
    ok(&mut db, "INSERT INTO p VALUES (1, 10), (2, 20)");

    // 把两行改成同一个主键:当场报错(不留到下一次查询才冒出来)
    let err = failure(&mut db, "UPDATE p SET k = 1 WHERE k = 2");
    assert!(matches!(err, DbError::DuplicateKey { .. }), "got {err:?}");
    assert!(err.to_string().contains("p_pkey"), "{err}");

    // 不撞的照旧,而且改完索引**立刻**是有效的(下一次查询连重建都省了)
    ok(&mut db, "UPDATE p SET k = 3 WHERE k = 2");
    assert_eq!(rows(&db, "SELECT v FROM p WHERE k = 3"), 1);
    assert_eq!(rows(&db, "SELECT v FROM p WHERE k = 2"), 0);
}

#[test]
fn a_unique_column_only_gets_an_access_path() {
    let mut db = fresh();
    // `UNIQUE` 与主键有意不同:只建索引,**不**强制唯一性
    ok(&mut db, "CREATE TABLE u (k INT UNIQUE, v INT)");

    assert_eq!(rows(&db, "SELECT v FROM u WHERE k = 1"), 0);
    ok(&mut db, "INSERT INTO u VALUES (1, 10)");
    ok(&mut db, "INSERT INTO u VALUES (1, 20)");
    assert_eq!(rows(&db, "SELECT v FROM u WHERE k = 1"), 2, "重复键放行");

    // 但访问路径是有的
    let plan = explain(&db, "SELECT v FROM u WHERE k = 1");
    assert!(plan.contains("Scan(u, index=[k], filter=[k = 1])"), "{plan}");
}

#[test]
fn the_implicit_index_cannot_be_dropped_on_its_own() {
    // 与 PostgreSQL 同口径:`PRIMARY KEY` 那条索引不能单独 `DROP INDEX`
    // (本版又没有 `ALTER TABLE ... DROP CONSTRAINT`,放行就等于"约束说没就没")
    let mut db = fresh();
    ok(&mut db, "CREATE TABLE p (k INT PRIMARY KEY)");

    let err = failure(&mut db, "DROP INDEX p_pkey");
    assert!(matches!(err, DbError::Unsupported { .. }), "got {err:?}");
    assert_eq!(db.indexes().len(), 1, "索引还在");

    // 显式建的索引照删不误
    ok(&mut db, "CREATE INDEX p_k ON p (k)");
    ok(&mut db, "DROP INDEX p_k");
    assert_eq!(db.indexes().len(), 1, "隐含的那条没被连坐");
}

#[test]
fn rebuild_indexes_is_idempotent_and_refuses_duplicates() {
    // "打开库时按 `Column::unique` 重建"那条路:幂等,而且盘上已经有重复行时
    // 建都建不出来(报错而不是悄悄放过)
    let mut db = fresh();
    created(&mut db, "CREATE TABLE p (k INT PRIMARY KEY, v INT)");
    assert_eq!(db.indexes().len(), 1);

    // 幂等:再调一次不新增
    let built = db.rebuild_indexes().unwrap();
    assert!(built.is_empty(), "{built:?}");
    assert_eq!(db.indexes().len(), 1);

    // 用户自己 `CREATE INDEX` 过的那一列,不会被再建一份
    let mut db = fresh();
    created(&mut db, "CREATE TABLE p (k INT PRIMARY KEY, v INT)");
    let before = db.indexes().len();
    ok(&mut db, "CREATE TABLE q (k INT PRIMARY KEY)");
    let added = db.rebuild_indexes().unwrap();
    assert!(added.is_empty(), "已经有的列不该重建:{added:?}");
    assert_eq!(db.indexes().len(), before + 1);

    // 表里已经有重复行:唯一索引建不出来
    let mut db = fresh();
    ok(&mut db, "CREATE TABLE d (k INT, v INT)");
    ok(&mut db, "INSERT INTO d VALUES (1, 10), (1, 20)");
    ok(&mut db, "CREATE INDEX d_k ON d (k)");
    let mut dup = fresh();
    ok(&mut dup, "CREATE TABLE d (k INT, v INT)");
    ok(&mut dup, "INSERT INTO d VALUES (1, 10), (1, 20)");
    let err = failure(&mut dup, "CREATE UNIQUE INDEX d_k ON d (k)");
    assert!(matches!(err, DbError::DuplicateKey { .. }), "got {err:?}");
    assert!(dup.indexes().is_empty(), "建不出来就不该留一条");
}

#[test]
fn a_multi_column_constraint_builds_no_index() {
    // 本版的索引是单列的:复合主键仍然只是"接受并记录"(口径见 ir/AGENTS.md)
    let mut db = fresh();
    let outcome = created(&mut db, "CREATE TABLE p (a INT, b INT, PRIMARY KEY (a, b))");

    match &outcome {
        Outcome::Created { indexes, .. } => assert!(indexes.is_empty(), "{indexes:?}"),
        other => panic!("expected Created, got {other:?}"),
    }
    assert!(db.indexes().is_empty(), "复合约束不建索引");

    // 重复的复合主键照样进得去(如实记录的"未强制"是真的没强制)
    ok(&mut db, "INSERT INTO p VALUES (1, 2)");
    ok(&mut db, "INSERT INTO p VALUES (1, 2)");
    assert_eq!(rows(&db, "SELECT a FROM p"), 2);
}

#[test]
fn an_implicit_index_name_follows_the_search_path() {
    let mut db = fresh();
    ok(
        &mut db,
        "CREATE SCHEMA study; SET search_path TO study; CREATE TABLE m (k INT PRIMARY KEY)",
    );

    assert_eq!(
        db.indexes(),
        vec![("study.m_pkey".to_string(), "study.m".to_string(), 0)],
        "索引名与表名是同一套生效名规则"
    );
    // `DROP SCHEMA ... CASCADE` 连隐含索引一起清(它已经在目录里,本来就该被覆盖)
    ok(&mut db, "DROP SCHEMA study CASCADE");
    assert!(db.indexes().is_empty(), "不该留悬挂索引");
}


// ------------------------------------------------- 外层键的索引探测(8.7)

/// 一张 `t(x INT, v INT)`,把 `row()`(整行)与 `value()`(单格)分开数.
struct ProbeTable {
    name: String,
    shape: ColumnsShape,
    rows: Vec<Vec<Value>>,
    rows_read: Rc<Cell<usize>>,
    cells_read: Rc<Cell<usize>>,
}

impl ProbeTable {
    fn new(name: &str, values: &[i32], rows: &Rc<Cell<usize>>, cells: &Rc<Cell<usize>>) -> Self {
        let shape = ColumnsShape::new(vec![
            Column::new("x", DataType::Int),
            Column::new("v", DataType::Int),
        ]);
        ProbeTable {
            name: name.to_string(),
            shape,
            rows: values
                .iter()
                .map(|v| vec![Value::Int(*v), Value::Int(v * 10)])
                .collect(),
            rows_read: Rc::clone(rows),
            cells_read: Rc::clone(cells),
        }
    }
}

impl TabShape for ProbeTable {
    fn columns(&self) -> &[Column] {
        self.shape.columns()
    }
}

impl Table for ProbeTable {
    fn name(&self) -> &str {
        &self.name
    }

    fn nrows(&self) -> usize {
        self.rows.len()
    }

    fn row(&self, r: usize) -> Option<Vec<Value>> {
        self.rows_read.set(self.rows_read.get() + 1);
        self.rows.get(r).cloned()
    }

    fn value(&self, r: usize, c: usize) -> Option<Value> {
        self.cells_read.set(self.cells_read.get() + 1);
        self.rows.get(r).and_then(|row| row.get(c)).cloned()
    }
}

#[test]
fn a_correlated_exists_uses_the_index_as_an_outer_key_probe() {
    // `tests.sql` 的 Test 10.3:`EXISTS (SELECT 1 FROM c WHERE c.tconst = b.tconst)`.
    // 内层只有一张表,**没有连接**可挂索引嵌套循环,所以这条谓词曾经退化成"外层每来
    // 一行就把内层整表扫一遍"--499999 行上跑不完.外层键的索引探测就是给它准备的:
    // 内层一次只取**命中**的那几行.
    let (rows, cells) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let mut db = SymbolTable::new();
    db.define("probe", ProbeTable::new("probe", &[1, 2, 3, 4, 5], &rows, &cells))
        .unwrap();
    // 两边那**一列同名**(`x`)是这条路的成立条件:键要搬到内表里同名的列上,
    // 见 `lower_outer_key`.
    ok(&mut db, "CREATE INDEX probe_x ON probe (x)");
    ok(&mut db, "CREATE TABLE outer_t (x INT PRIMARY KEY, v INT)");
    ok(
        &mut db,
        "INSERT INTO outer_t VALUES (1, 10), (2, 20), (99, 30)",
    );

    let before = rows.get();
    let plan = compile(
        "SELECT x FROM outer_t AS b WHERE EXISTS (SELECT 1 FROM probe WHERE x = b.x)",
        &db,
    )
    .unwrap();
    let out = execute(&plan, &db).unwrap();

    assert_eq!(out.nrows(), 2, "只有 1 与 2 配得上");
    assert_eq!(
        rows.get() - before,
        2,
        "索引探测:只为**命中**的那两行取行(顺序扫描每条外层行都要拉满整表)"
    );
}

#[test]
fn an_update_that_touches_no_key_column_does_not_rescan_for_uniqueness() {
    // `update_*.sql` 里的 `UPDATE` 全是"改无关列"(`runtime_minutes + 1` 之类).
    // 那种语句一个唯一列都没碰到,唯一性**不可能**被破坏,所以"改完之后按新数据
    // 重建唯一索引查一遍重"那一步必须整段省掉:它是 O(整表)的,无脑做会把"改几行"
    // 拖成"扫全表"(实测:1000 行的 `UPDATE` 从 91 秒掉回毫秒量级).
    //
    // 判据:`v` 列上**只有一条**非唯一索引,而 `UPDATE` 改的是 `v`;那条索引在
    // `UPDATE` 里照样整体作废,但**唯一列一个都没碰**,所以一次都不该重建它
    // (重建要逐行读 `v` 那一列,`value()` 的次数会当场涨上去).
    let (mut db, counts) = counting(&[1, 2, 3, 4, 5]);
    ok(&mut db, "CREATE INDEX t_x ON t (x)");
    let after_create = counts.cells.get();

    ok(&mut db, "UPDATE t SET x = x + 10");

    // 单格读只来自 `UPDATE` 自己的 `row()`(它走 `Table::row`,不是 `value`),
    // 所以重建一次索引会在这里留下 +5.
    assert_eq!(
        counts.cells.get(),
        after_create,
        "唯一列没被碰到,就不该有人为了查重去读它"
    );

}
