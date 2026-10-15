//! 持久化层:把表写进介质,再从介质读回来.
//!
//! 本层建立在 [`crate::db::table::Table`] 抽象之上,**不关心**表具体是物理表
//! 还是虚表:`save` 吃的是 `&dyn Table`,因此将来连接表的结果也能直接落盘.
//!
//! # 磁盘布局
//!
//! 一张表对应**成对**的两个文件,放在同一个目录里:
//!
//! ```text
//! <dir>/<name>.csv           数据:纯列名表头 + 数据行
//! <dir>/<name>.schema.json   模式:{version, name, columns:[{name, ty}]}
//! ```
//!
//! 之所以把类型信息放在旁路文件里,是为了让数据文件保持**标准 CSV**(能被
//! 其它工具直接读),同时又不丢掉 schema.
//!
//! 一份表的落盘还分**两步**:`stage` 把两份文件写成暂存名
//! (`<name>.csv.new` / `<name>.schema.json.new`),之后再逐个 rename 到正式位置
//! ([`TablePath::staged_data_path`]).单表提交两步一起做([`Persister::save`]);
//! 多表提交由 `usr::service` 把所有表都暂存完,写下提交意图之后才一起推广,
//! 于是**崩在推广的中途也能补完**.
//!
//! "整库"被预留为**一个目录 = 一个库**:同一个目录下可以放任意多组
//! `<name>.csv` + `<name>.schema.json`,将来要枚举库里的表只需扫一遍目录.
//! 本模块目前**不提供** catalog 类型.
//!
//! # 两份文件的一致性
//!
//! 旁路方案的固有代价是两份文件可能不同步.`load` 会**交叉校验** CSV 表头与
//! 模式文件的列名(含顺序),不一致直接报 [`crate::db::table::DbError::Format`],
//! 绝不静默读出错误的表.
//!
//! 布局的权威归属:**模式文件里的 `name` 是表的逻辑名**,文件主干名只用来定位.

pub mod csv;

pub use csv::CsvPersister;

use std::path::{Path, PathBuf};

use crate::db::phys::PhysTab;
use crate::db::table::{Result, Table};

/// 一张表在磁盘上的位置.
///
/// 由"目录 + 表名"推出数据文件与模式文件两个路径,磁盘布局的规则只此一处.
///
/// ```
/// use ydbms::pers::TablePath;
///
/// let at = TablePath::new("/tmp/db", "f");
/// assert!(at.data_path().ends_with("f.csv"));
/// assert!(at.schema_path().ends_with("f.schema.json"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePath {
    data: PathBuf,
    schema: PathBuf,
}

impl TablePath {
    /// 用目录与名字定位一张表.
    pub fn new(dir: impl AsRef<Path>, name: &str) -> Self {
        let dir = dir.as_ref();
        TablePath {
            data: dir.join(format!("{name}.csv")),
            schema: dir.join(format!("{name}.schema.json")),
        }
    }

    /// 由数据文件路径反推(扫目录拿到 `*.csv` 之后用).
    pub fn from_data_path(data: impl Into<PathBuf>) -> Self {
        let data = data.into();
        let schema = data.with_extension("schema.json");
        TablePath { data, schema }
    }

    /// 数据文件路径.
    pub fn data_path(&self) -> &Path {
        &self.data
    }

    /// 模式文件路径.
    pub fn schema_path(&self) -> &Path {
        &self.schema
    }

    /// **暂存**的数据文件路径(`<name>.csv.new`).
    ///
    /// 一次多表提交的原子性靠它:先把每一张表都写成暂存文件,都写完了才在库目录里
    /// 落下"提交意图"(`commit.json`),然后才逐个 rename 进正式位置.于是崩在
    /// rename 的中途也**能补完**--新数据早就全在暂存文件里了(见
    /// [`Database::commit`](crate::usr::service::Database::commit)).
    ///
    /// 后缀是**追加**上去的,不是 `with_extension`:表名可以带点
    /// (`study.movies.csv`),`with_extension` 会把它切成 `study.movies.new`.
    pub fn staged_data_path(&self) -> PathBuf {
        append_suffix(&self.data, ".new")
    }

    /// **暂存**的模式文件路径(`<name>.schema.json.new`).
    ///
    /// 与 [`staged_data_path`](TablePath::staged_data_path) 成对;两份文件一起被
    /// 推广,中途崩了也只是补完剩下那一份(两份都指向同一份新数据).
    pub fn staged_schema_path(&self) -> PathBuf {
        append_suffix(&self.schema, ".new")
    }
}

/// 在文件名**后面追加**后缀(不替换扩展名,见 [`TablePath::staged_data_path`]).
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// 把表持久化到介质,以及从介质读回的接口.
///
/// 两个方法都是 dyn-compatible 的,所以可以放进 `Box<dyn Persister>`,将来用
/// 统一的入口挂载不同格式(CSV,二进制......).
///
/// ```
/// use ydbms::db::table::{Column, ColumnsShape, DataType, Table, Value};
/// use ydbms::db::PhysTab;
/// use ydbms::pers::{CsvPersister, Persister, TablePath};
///
/// let shape = ColumnsShape::new(vec![
///     Column::new("x", DataType::Int),
///     Column::new("value", DataType::Int),
/// ]);
/// let mut tab = PhysTab::new("f", &shape);
/// tab.insert(vec![Value::Int(-5), Value::Int(26)]).unwrap();
///
/// let dir = std::env::temp_dir().join("ydbms-doc-example");
/// let at = TablePath::new(&dir, "f");
/// let back = CsvPersister::new().save(&tab, &at).and_then(|()| CsvPersister::new().load(&at)).unwrap();
/// assert_eq!(back.row(0), Some(vec![Value::Int(-5), Value::Int(26)]));
/// # let _ = std::fs::remove_dir_all(&dir);
/// ```
pub trait Persister {
    /// 把**任意**表写进 `at`.
    ///
    /// 表的逻辑名取自 [`Table::name`],会写进模式文件.
    fn save(&self, table: &dyn Table, at: &TablePath) -> Result<()>;

    /// 从 `at` 读回一张物理表.
    fn load(&self, at: &TablePath) -> Result<PhysTab>;
}
