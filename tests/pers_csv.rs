//! [`CsvPersister`] 的 testbench.
//!
//! 主线是从磁盘"存→读"一张物理表并逐格核对数值;此外覆盖了旁路模式文件方案
//! 特有的失败模式:两份文件不同步,模式缺失,数据被改坏.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ydbms::db::table::{Column, ColumnsShape, DataType, DbError, Table, TabShape, Unique, Value};
use ydbms::db::PhysTab;
use ydbms::pers::{CsvPersister, Persister, TablePath};

const X_MIN: i32 = -5;
const X_MAX: i32 = 5;
/// 行数:x 取遍闭区间 `[-5, 5]`.
const N: usize = (X_MAX - X_MIN + 1) as usize;

/// f(x) = x² + 1
fn f(x: i32) -> i32 {
    x * x + 1
}

/// f'(x) = 2x
fn df(x: i32) -> i32 {
    2 * x
}

fn quadratic_shape() -> ColumnsShape {
    ColumnsShape::new(vec![
        Column::new("x", DataType::Int),
        Column::new("value", DataType::Int),
        Column::new("drvd", DataType::Int),
    ])
}

/// 建一张 `(x, f(x), f'(x))` 物理表,`x ∈ [-5, 5]`.
fn build_quadratic_table(name: &str) -> PhysTab {
    let shape = quadratic_shape();
    let mut tab = PhysTab::new(name, &shape);
    for x in X_MIN..=X_MAX {
        tab.insert(vec![Value::from(x), Value::from(f(x)), Value::from(df(x))])
            .expect("insert should succeed");
    }
    tab
}

/// 自清理的临时目录.不引入 `tempfile` 依赖,用进程号 + 计数器保证并行唯一.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("ydbms-pers-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
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

#[test]
fn round_trip_quadratic_table() {
    let dir = TempDir::new("roundtrip");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();

    let saved = build_quadratic_table("f");
    persister.save(&saved, &at).expect("save");

    let loaded = persister.load(&at).expect("load");

    assert_eq!(loaded.name(), "f");
    assert_eq!(loaded.nrows(), N);
    assert_eq!(loaded.arity(), saved.arity());
    assert_eq!(loaded.columns(), saved.columns(), "形状应当原样回来");

    for r in 0..N {
        assert_eq!(loaded.row(r), saved.row(r), "第 {r} 行");
    }
    assert_eq!(loaded.row(N), None);
}

#[test]
fn writes_the_expected_file_pair() {
    let dir = TempDir::new("pair");
    let at = TablePath::new(dir.path(), "f");
    CsvPersister::new()
        .save(&build_quadratic_table("f"), &at)
        .unwrap();

    assert!(at.data_path().exists());
    assert!(at.schema_path().exists());

    let data = std::fs::read_to_string(at.data_path()).unwrap();
    assert_eq!(data.lines().next(), Some("x,value,drvd"));
    assert_eq!(data.lines().count(), N + 1, "表头 + 11 行数据");

    let raw = std::fs::read_to_string(at.schema_path()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(json["version"].as_u64(), Some(1));
    assert_eq!(json["name"].as_str(), Some("f"));
    assert_eq!(json["columns"][0]["name"].as_str(), Some("x"));
    assert_eq!(json["columns"][1]["name"].as_str(), Some("value"));
    assert_eq!(json["columns"][2]["name"].as_str(), Some("drvd"));
    assert_eq!(json["columns"][1]["ty"].as_str(), Some("INT"));
}

#[test]
fn a_primary_key_marker_survives_the_schema_file() {
    // TODO 8.7:`Column::unique` 是"这一列上本该有一条索引"的落盘标记(索引本身
    // 不落盘),所以它必须能原样往返;而 `Unique::None` 的列**不写**这个键,
    // 旧文件的字节因此一个都不变.
    let dir = TempDir::new("unique");
    let at = TablePath::new(dir.path(), "p");
    let shape = ColumnsShape::new(vec![
        Column::new("k", DataType::Int).with_unique(Unique::Key),
        Column::new("u", DataType::Int).with_unique(Unique::Unique),
        Column::new("v", DataType::Int),
    ]);
    let mut saved = PhysTab::new("p", &shape);
    saved
        .insert(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        .unwrap();

    let persister = CsvPersister::new();
    persister.save(&saved, &at).unwrap();

    // 主键与 `UNIQUE` 分得开,没标记的那一列**没有这个键**
    let raw = std::fs::read_to_string(at.schema_path()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["columns"][0]["unique"].as_str(), Some("key"));
    assert_eq!(json["columns"][1]["unique"].as_str(), Some("unique"));
    assert!(json["columns"][2].get("unique").is_none(), "{json}");

    let loaded = persister.load(&at).unwrap();
    assert_eq!(loaded.columns(), saved.columns(), "标记要原样回到列定义里");
}

#[test]
fn csv_quoting_round_trips_awkward_column_names() {    let dir = TempDir::new("quoting");
    let at = TablePath::new(dir.path(), "quoted");

    // 目前只有 INT,值里不可能出现逗号;列名是唯一能走到 CSV 引号逻辑的地方.
    let shape = ColumnsShape::new(vec![
        Column::new("odd,name", DataType::Int),
        Column::new("say\"hi", DataType::Int),
    ]);
    let mut saved = PhysTab::new("quoted", &shape);
    saved
        .insert(vec![Value::Int(1), Value::Int(2)])
        .expect("insert");

    let persister = CsvPersister::new();
    persister.save(&saved, &at).unwrap();

    let data = std::fs::read_to_string(at.data_path()).unwrap();
    assert_eq!(
        data.lines().next(),
        Some(r#""odd,name","say""hi""#),
        "逗号与双引号应当按 RFC 4180 转义"
    );

    let loaded = persister.load(&at).unwrap();
    assert_eq!(loaded.columns(), saved.columns());
    assert_eq!(loaded.row(0), Some(vec![Value::Int(1), Value::Int(2)]));
}

#[test]
fn detects_header_schema_mismatch() {
    let dir = TempDir::new("mismatch");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();
    persister.save(&build_quadratic_table("f"), &at).unwrap();

    // 只改旁路模式文件:两份文件从此描述的不是同一张表
    let raw = std::fs::read_to_string(at.schema_path()).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json["columns"][0]["name"] = serde_json::Value::from("renamed");
    std::fs::write(
        at.schema_path(),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .unwrap();

    let err = persister.load(&at).unwrap_err();
    assert!(matches!(err, DbError::Format { .. }), "got {err:?}");
    assert!(
        err.to_string().contains("does not match schema"),
        "错误信息应当说明两份文件对不上:{err}"
    );
}

#[test]
fn missing_schema_file_is_reported() {
    let dir = TempDir::new("noschema");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();
    persister.save(&build_quadratic_table("f"), &at).unwrap();

    std::fs::remove_file(at.schema_path()).unwrap();

    let err = persister.load(&at).unwrap_err();
    assert!(matches!(err, DbError::MissingSchema { .. }), "got {err:?}");
    assert!(at.data_path().exists(), "数据文件还在,只是模式丢了");
}

#[test]
fn rejects_non_integer_field() {
    let dir = TempDir::new("badint");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();
    persister.save(&build_quadratic_table("f"), &at).unwrap();

    // 把第一条数据(表头之后,即文件第 2 行)的第二列改成非整数
    let data = std::fs::read_to_string(at.data_path()).unwrap();
    let mut lines: Vec<&str> = data.lines().collect();
    lines[1] = "-5,abc,-10";
    std::fs::write(at.data_path(), format!("{}\n", lines.join("\n"))).unwrap();

    let (line, message) = match persister.load(&at).unwrap_err() {
        DbError::Format { line, message, .. } => (line, message),
        other => panic!("expected Format, got {other:?}"),
    };

    assert_eq!(line, Some(2), "行号应当指向文件第 2 行");
    assert!(message.contains("INT"), "{message}");
}

#[test]
fn rejects_ragged_rows() {
    let dir = TempDir::new("ragged");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();
    persister.save(&build_quadratic_table("f"), &at).unwrap();

    let data = std::fs::read_to_string(at.data_path()).unwrap();
    let mut lines: Vec<&str> = data.lines().collect();
    lines.push("-1,2"); // 少一列
    std::fs::write(at.data_path(), format!("{}\n", lines.join("\n"))).unwrap();

    let err = persister.load(&at).unwrap_err();
    assert!(matches!(err, DbError::Format { .. }), "got {err:?}");
}

#[test]
fn empty_table_round_trips() {
    let dir = TempDir::new("empty");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();

    let saved = PhysTab::new("f", &quadratic_shape());
    persister.save(&saved, &at).unwrap();

    let loaded = persister.load(&at).unwrap();
    assert_eq!(loaded.nrows(), 0);
    assert_eq!(loaded.arity(), 3, "没有数据也不该丢形状");
    assert_eq!(loaded.columns(), saved.columns());
    assert_eq!(loaded.name(), "f");
}

#[test]
fn save_overwrites_previous_contents() {
    let dir = TempDir::new("overwrite");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();

    persister.save(&build_quadratic_table("f"), &at).unwrap();

    let mut small = PhysTab::new("f", &quadratic_shape());
    small
        .insert(vec![Value::Int(0), Value::Int(1), Value::Int(0)])
        .unwrap();
    small
        .insert(vec![Value::Int(1), Value::Int(2), Value::Int(2)])
        .unwrap();
    persister.save(&small, &at).unwrap();

    let loaded = persister.load(&at).unwrap();
    assert_eq!(loaded.nrows(), 2, "旧内容不应残留");
    assert_eq!(
        loaded.row(1),
        Some(vec![Value::Int(1), Value::Int(2), Value::Int(2)])
    );
}

#[test]
fn multiple_tables_share_one_directory() {
    let dir = TempDir::new("multidir");
    let persister = CsvPersister::new();

    let f_at = TablePath::new(dir.path(), "f");
    let g_at = TablePath::new(dir.path(), "g");

    let g_shape = ColumnsShape::new(vec![
        Column::new("id", DataType::Int),
        Column::new("parent", DataType::Int),
    ]);
    let mut g = PhysTab::new("g", &g_shape);
    g.insert(vec![Value::Int(1), Value::Int(0)]).unwrap();

    persister.save(&build_quadratic_table("f"), &f_at).unwrap();
    persister.save(&g, &g_at).unwrap();

    let f_loaded = persister.load(&f_at).unwrap();
    let g_loaded = persister.load(&g_at).unwrap();

    assert_eq!(f_loaded.name(), "f");
    assert_eq!(f_loaded.arity(), 3);
    assert_eq!(f_loaded.nrows(), N);

    assert_eq!(g_loaded.name(), "g");
    assert_eq!(g_loaded.arity(), 2);
    assert_eq!(g_loaded.row(0), Some(vec![Value::Int(1), Value::Int(0)]));
}

#[test]
fn schema_name_wins_over_file_stem() {
    let dir = TempDir::new("namewins");
    let at = TablePath::new(dir.path(), "quadratic"); // 文件叫 quadratic.csv
    let persister = CsvPersister::new();

    persister.save(&build_quadratic_table("f"), &at).unwrap(); // 表名叫 f
    assert!(at.data_path().ends_with("quadratic.csv"));

    let loaded = persister.load(&at).unwrap();
    assert_eq!(loaded.name(), "f", "逻辑表名以模式文件为准");
}

#[test]
fn works_behind_a_trait_object() {
    let dir = TempDir::new("dyn");
    let at = TablePath::new(dir.path(), "f");
    let persister: Box<dyn Persister> = Box::new(CsvPersister::new());

    persister.save(&build_quadratic_table("f"), &at).unwrap();
    let loaded = persister.load(&at).unwrap();

    assert_eq!(loaded.nrows(), N);
    assert_eq!(loaded.value(0, 1), Some(Value::Int(26)));
}

#[test]
fn loaded_table_is_writable() {
    let dir = TempDir::new("writable");
    let at = TablePath::new(dir.path(), "f");
    let persister = CsvPersister::new();
    persister.save(&build_quadratic_table("f"), &at).unwrap();

    let mut loaded = persister.load(&at).unwrap();
    let idx = loaded
        .insert(vec![Value::Int(6), Value::Int(37), Value::Int(12)])
        .expect("读回来的确实是一张可写的物理表");

    assert_eq!(idx, N);
    assert_eq!(loaded.nrows(), N + 1);
    assert_eq!(loaded.value(N, 1), Some(Value::Int(37)));
}

#[test]
fn no_stray_temp_files_left_behind() {
    let dir = TempDir::new("tmpfiles");
    let at = TablePath::new(dir.path(), "f");
    CsvPersister::new()
        .save(&build_quadratic_table("f"), &at)
        .unwrap();

    let mut entries: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();

    assert_eq!(entries, vec!["f.csv".to_string(), "f.schema.json".to_string()]);
}

#[test]
fn table_path_layout() {
    let at = TablePath::new("/tmp/ydbms-layout", "f");

    assert_eq!(at.data_path(), Path::new("/tmp/ydbms-layout/f.csv"));
    assert_eq!(
        at.schema_path(),
        Path::new("/tmp/ydbms-layout/f.schema.json")
    );

    // 从数据文件路径反推(扫目录之后用),应当得到同一个位置
    assert_eq!(TablePath::from_data_path(at.data_path()), at);
}
