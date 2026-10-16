//! 用"标准 CSV + 旁路 JSON 模式文件"实现的持久化.
//!
//! 数据用 [`csv`] crate 读写(引号,逗号,换行等规则都由它处理),类型信息写成
//! 同目录下的 `<name>.schema.json`.布局与一致性约定见 [`crate::pers`].
//!
//! 落盘分**暂存**([`CsvPersister::stage`],写 `<name>.csv.new`)与**推广**
//! (`promote`,rename 进正式位置)两步:单表提交两步连着做
//! ([`Persister::save`]),多表提交由 `usr::service` 攒着一起做,于是崩在中间
//! 也能补完(原因见 [`crate::pers`] 的文件头).
//!
//! # 变更日志(TODO 7.7)
//!
//! 一次提交要把整张表重写一遍是 O(表) 的(实测 50 万行 / 38 MB 的表上 200~270 ms,
//! 改一行也是这个价钱),所以改过的那几行**追加**到 `<name>.log` 里就完事:
//!
//! ```text
//! A,<行号>,<字段>...      往表尾加一行(行号必须正好是当前行数)
//! U,<行号>,<字段>...      替换第 <行号> 行
//! ```
//!
//! 字段用**数据文件那一套**(NULL 是 `\N`,以 `\` 开头的文本多写一个 `\`,见
//! [`crate::db::table::DataType::from_csv_field`]),于是值怎么落盘只有一处口径.
//! 载入时把日志按顺序重放到基表上([`CsvPersister::replay_log`]);**已提交到哪一行**
//! 记在库目录的 `manifest.json` 里(见 `usr::service`),多出来的尾巴是没越过提交点
//! 的那些,打开库时被截掉.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{Persister, TableDelta, TablePath};
use crate::db::phys::PhysTab;
use crate::db::table::{
    Column, ColumnsShape, DbError, Result, Row, Table, TabShape, Value,
};

/// 模式文件的格式版本.
///
/// `load` 遇到不认识的版本会直接报错,而不是按当前格式硬读--宁可失败也不静默
/// 读错数据.将来改格式时靠它做迁移.
const SCHEMA_VERSION: u32 = 1;

/// 变更日志里"追加一行"的记录标记.
const LOG_APPEND: &str = "A";

/// 变更日志里"替换一行"的记录标记.
const LOG_UPDATE: &str = "U";

/// 旁路模式文件的内容.
#[derive(Debug, Serialize, Deserialize)]
struct SchemaFile {
    /// 格式版本,见 [`SCHEMA_VERSION`].
    version: u32,
    /// 表的逻辑名.
    name: String,
    /// 列定义,顺序即 CSV 的列序.
    columns: Vec<Column>,
}

/// 以 CSV 存放数据,以旁路 JSON 存放模式的 [`Persister`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CsvPersister;

impl CsvPersister {
    /// 新建一个实现.它是无状态的,等价于 [`CsvPersister::default`].
    pub fn new() -> Self {
        CsvPersister
    }

    /// 把表写成**暂存**的一对文件(`<name>.csv.new` / `<name>.schema.json.new`).
    ///
    /// 暂存文件没有任何读者,所以这里**直接写**(不必先写 `.tmp` 再改名):写坏了
    /// 也只坏在暂存名上,恢复/清扫会把它删掉.它**不是**一次提交--推广那一步
    /// (rename 到正式位置,见 [`TablePath::staged_data_path`])才是.
    ///
    /// 单表提交([`Persister::save`])就是"暂存 + 立刻推广".
    pub fn stage(&self, table: &dyn Table, at: &TablePath) -> Result<()> {
        let data_path = at.staged_data_path();
        let schema_path = at.staged_schema_path();

        if let Some(dir) = data_path.parent() {
            fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }

        write_csv(&data_path, table)?;
        // 模式文件写坏了要把数据那一份也删掉:留一份孤零零的暂存数据没有意义,
        // 清扫虽然也会删,但在这里就删掉更干净(也免得下次暂存只覆盖了一半).
        if let Err(e) = write_schema(&schema_path, table, at.schema_path()) {
            remove_quietly(&data_path);
            return Err(e);
        }
        Ok(())
    }

    /// 把 `delta` 里的改动**追加**到 `<表>.log`(TODO 7.7).
    ///
    /// 只写改过的那几行:一次 `COPY` 灌进来的那一段用一个起点表示,原地改过的行号
    /// 一个一个写.行号是**绝对行号**,所以重放不依赖"写的时候表里有多少行".
    ///
    /// 追加式写:文件不重写(重写就又变成 O(表) 了),调用方(`usr::service` 的提交点)
    /// 把"写到哪儿算数"记在 `manifest.json` 里.
    pub fn append_delta(
        &self,
        table: &dyn Table,
        at: &TablePath,
        delta: &TableDelta,
        committed: usize,
    ) -> Result<usize> {
        let path = at.log_path();
        // **先截到已提交的条数**:上次崩在半路留下的尾巴,或者上一次整理之后残留的
        // 旧记录,都在这里被清掉--不先清就往后追加,清单里的条数会对到**旧**记录上
        // (那是安静的错答案)
        truncate_log(at, committed)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| io_err(&path, e))?;

        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(file);
        let mut appended = 0usize;

        if let Some(from) = delta.appended_from {
            for r in from..table.nrows() {
                let row = table.row(r).ok_or_else(|| DbError::IrRuntime {
                    node: None,
                    message: format!("{}", tr!("pers.csv.001", arg0 = r, arg1 = table.name())),
                })?;
                write_log_record(&mut writer, &path, LOG_APPEND, r, &row)?;
                appended += 1;
            }
        }
        for r in &delta.updated {
            // 这一轮里**追加**进来的行已经带着最终内容进去了(`A` 那条就是在提交
            // 这一刻按最终内容写的),不必再写一遍;越界的行号(理论上不会有)也跳过.
            if delta.appended_from.is_some_and(|from| *r >= from) || *r >= table.nrows() {
                continue;
            }
            let row = table.row(*r).ok_or_else(|| DbError::IrRuntime {
                node: None,
                message: format!("{}", tr!("pers.csv.001", arg0 = r, arg1 = table.name())),
            })?;
            write_log_record(&mut writer, &path, LOG_UPDATE, *r, &row)?;
            appended += 1;
        }

        writer.flush().map_err(|e| io_err(&path, e))?;
        Ok(appended)
    }

    /// 把 `<表>.log` 重放到 `table` 上(载入时用,TODO 7.7).
    ///
    /// 日志不存在就是没改过,不算错.整份文件都认(打开库时已经把它截到**已提交**
    /// 的行数了,见 `usr::service` 的恢复那一步);读到半截或者行号对不上都报
    /// [`DbError::Format`] -- 宁可开不了库,也不要读出一张错位的表.
    pub fn replay_log(&self, table: &mut PhysTab, at: &TablePath) -> Result<()> {
        let path = at.log_path();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(&path, e)),
        };

        let columns = table.columns().to_vec();
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(file);

        for record in reader.records() {
            let record = record.map_err(|e| csv_err(&path, e))?;
            let line = record.position().map(|p| p.line() as usize);
            let bad = |message: String| DbError::Format {
                path: path.display().to_string(),
                line,
                message,
            };

            let op = record.get(0).unwrap_or_default().to_string();
            let rid: usize = record
                .get(1)
                .and_then(|text| text.parse().ok())
                .ok_or_else(|| bad(tr!("pers.csv.002").into_owned().to_string()))?;

            let mut row: Row = Vec::with_capacity(columns.len());
            for (field, column) in record.iter().skip(2).zip(columns.iter()) {
                row.push(column.ty.from_csv_field(field).map_err(|e| {
                    bad(format!(
                        "{}",
                        tr!("pers.csv.003", arg0 = column.name, arg1 = e)
                    ))
                })?);
            }
            if row.len() != columns.len() {
                return Err(bad(format!(
                    "{}",
                    tr!("pers.csv.004", arg0 = row.len(), arg1 = columns.len())
                )));
            }

            match op.as_str() {
                LOG_APPEND => {
                    if rid != table.nrows() {
                        return Err(bad(format!(
                            "{}",
                            tr!("pers.csv.005", arg0 = rid, arg1 = table.nrows())
                        )));
                    }
                    table.insert(row)?;
                }
                LOG_UPDATE => {
                    if rid >= table.nrows() {
                        return Err(bad(format!(
                            "{}",
                            tr!("pers.csv.006", arg0 = rid, arg1 = table.nrows())
                        )));
                    }
                    table.update_row(rid, row)?;
                }
                other => {
                    return Err(bad(format!("{}", tr!("pers.csv.007", arg0 = other))));
                }
            }
        }

        Ok(())
    }
}

/// 把 `<表>.log` 截到前 `lines` 条**完整**的记录(TODO 7.7).
///
/// 日志是追加式的,而"到哪儿算数"记在库目录的 `manifest.json` 里(见
/// `usr::service`):没越过提交点的尾巴,以及整理之后残留的旧记录,都由它截掉.
///
/// 返回截断之后的字节数.文件不在而清单说 0 条不算错;清单说有 n 条而文件里没有
/// 那么多完整记录**要报错**--那是坏的现场,宁可开不了库.
pub fn truncate_log(at: &TablePath, lines: usize) -> Result<u64> {
    let path = at.log_path();
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if lines == 0 {
                return Ok(0);
            }
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: None,
                message: format!("{}", tr!("pers.csv.008", arg0 = lines)),
            });
        }
        Err(e) => return Err(io_err(&path, e)),
    };

    let offset = log_offset(&file, &path, lines)?;
    let size = file.metadata().map_err(|e| io_err(&path, e))?.len();
    if size > offset {
        file.set_len(offset).map_err(|e| io_err(&path, e))?;
    }
    Ok(offset)
}

/// 第 `lines` 条记录之后的字节偏移(不足那么多**完整**行就报错).
fn log_offset(file: &File, path: &Path, lines: usize) -> Result<u64> {
    if lines == 0 {
        return Ok(0);
    }
    let mut reader = std::io::BufReader::new(file);
    let mut buffer = Vec::new();
    let mut offset = 0u64;
    for seen in 0..lines {
        buffer.clear();
        let read = std::io::BufRead::read_until(&mut reader, b'\n', &mut buffer)
            .map_err(|e| io_err(path, e))?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            return Err(DbError::Format {
                path: path.display().to_string(),
                line: Some(seen + 1),
                message: format!("{}", tr!("pers.csv.009", arg0 = lines)),
            });
        }
        offset += read as u64;
    }
    Ok(offset)
}

/// 写一条日志记录:`[标记, 行号, 字段...]`.
fn write_log_record(
    writer: &mut csv::Writer<File>,
    path: &Path,
    op: &str,
    rid: usize,
    row: &Row,
) -> Result<()> {
    let fields: Vec<String> = row.iter().map(Value::to_csv_field).collect();
    writer
        .write_record(
            std::iter::once(op.to_string())
                .chain(std::iter::once(rid.to_string()))
                .chain(fields),
        )
        .map_err(|e| csv_err(path, e))
}


/// 把暂存的一对文件推广到正式位置(各一次 rename,同文件系统内是原子的).
///
/// 两份文件**各 rename 一次**,所以中途崩掉可能只推广了一半--没关系:
/// 提交记录(`manifest.json`)里还记着这次要推广谁,恢复时把还没搬的搬完就对了
/// (两份都指向新数据).
pub(crate) fn promote(at: &TablePath) -> Result<()> {
    for path in [at.data_path(), at.schema_path()] {
        let staged = match path == at.data_path() {
            true => at.staged_data_path(),
            false => at.staged_schema_path(),
        };
        if !staged.exists() {
            // 已经搬过了(恢复重放时会遇到)
            continue;
        }
        fs::rename(&staged, path).map_err(|e| io_err(path, e))?;
    }
    Ok(())
}

impl Persister for CsvPersister {
    fn save(&self, table: &dyn Table, at: &TablePath) -> Result<()> {
        // 先写暂存文件再改名:读者不会看到一个写了一半的文件(与过去完全一样,
        // 只是"临时文件"改叫"暂存文件",恢复时也认它).
        self.stage(table, at)?;
        promote(at)
    }

    fn load(&self, at: &TablePath) -> Result<PhysTab> {
        let data_path = at.data_path();
        let schema_path = at.schema_path();

        // 1) 模式文件
        let raw = match fs::read_to_string(schema_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DbError::MissingSchema {
                    path: schema_path.display().to_string(),
                })
            }
            Err(e) => return Err(io_err(schema_path, e)),
        };
        let schema: SchemaFile = serde_json::from_str(&raw).map_err(|e| DbError::Format {
            path: schema_path.display().to_string(),
            line: None,
            message: format!("schema file is not a valid schema: {}", e),
        })?;
        if schema.version != SCHEMA_VERSION {
            return Err(DbError::Format {
                path: schema_path.display().to_string(),
                line: None,
                message: format!(
                    "unsupported schema version {} (this build understands {})",
                    schema.version, SCHEMA_VERSION
                ),
            });
        }
        let shape = ColumnsShape::new(schema.columns.clone());

        // 2) 数据文件
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(false)
            .from_path(data_path)
            .map_err(|e| csv_err(data_path, e))?;

        // 3) 交叉校验:两份文件必须描述同一张表,否则宁可报错也不读错.
        let header = reader.headers().map_err(|e| csv_err(data_path, e))?;
        let expected: Vec<&str> = shape.columns().iter().map(|c| c.name.as_str()).collect();
        let found: Vec<&str> = header.iter().collect();
        if found != expected {
            return Err(DbError::Format {
                path: data_path.display().to_string(),
                line: Some(1),
                message: format!(
                    "CSV header {:?} does not match schema columns {:?}",
                    found, expected
                ),
            });
        }

        // 4) 逐行还原.表名以模式文件为准.
        let mut table = PhysTab::new(schema.name, &shape);
        for record in reader.records() {
            let record = record.map_err(|e| csv_err(data_path, e))?;
            let line = record.position().map(|p| p.line() as usize);

            let mut row: Row = Vec::with_capacity(shape.arity());
            for (i, col) in shape.columns().iter().enumerate() {
                // flexible(false) 已经挡住字段数不齐,这里只是防御.
                let field = record.get(i).ok_or_else(|| DbError::Format {
                    path: data_path.display().to_string(),
                    line,
                    message: format!("row has {} fields, expected {}", record.len(), shape.arity()),
                })?;
                row.push(parse_field(field, col, data_path, line)?);
            }
            table.insert(row)?;
        }

        Ok(table)
    }
}

/// 把表写成 CSV:先写一条纯列名表头,再逐行写数据.
fn write_csv(path: &Path, table: &dyn Table) -> Result<()> {
    let mut writer = csv::Writer::from_path(path).map_err(|e| csv_err(path, e))?;

    writer
        .write_record(table.columns().iter().map(|c| c.name.as_str()))
        .map_err(|e| csv_err(path, e))?;

    for row in table.iter_rows() {
        // 挡住实现有问题的表,别把坏数据写进文件.
        table.validate(&row)?;
        writer
            .write_record(row.iter().map(|v| v.to_csv_field()))
            .map_err(|e| csv_err(path, e))?;
    }

    // Writer::flush 返回的是 io::Result,不是 csv::Error.
    writer.flush().map_err(|e| io_err(path, e))?;
    Ok(())
}

/// 把模式写成 JSON.
fn write_schema(path: &Path, table: &dyn Table, real_path: &Path) -> Result<()> {
    let schema = SchemaFile {
        version: SCHEMA_VERSION,
        name: table.name().to_string(),
        columns: table.columns().to_vec(),
    };
    let json = serde_json::to_string_pretty(&schema).map_err(|e| DbError::Format {
        path: real_path.display().to_string(),
        line: None,
        message: format!("cannot serialize schema: {}", e),
    })?;

    fs::write(path, format!("{}\n", json)).map_err(|e| io_err(path, e))
}

/// 按列类型把 CSV 的一个字段解析成 [`Value`].
///
/// 解析是**严格**的:不接受前后空白,`" 5"` 会被当作坏数据.
fn parse_field(field: &str, column: &Column, path: &Path, line: Option<usize>) -> Result<Value> {
    // 类型怎么解析,typmod 怎么强制,全在 `DataType::parse` 里;这里只负责
    // 把失败包装成"文件内容有问题"的 Format 错误.
    column.ty.from_csv_field(field).map_err(|e| DbError::Format {
        path: path.display().to_string(),
        line,
        message: format!("column `{}`: {}", column.name, e),
    })
}

fn remove_quietly(path: &Path) {
    let _ = fs::remove_file(path);
}

fn io_err(path: &Path, e: std::io::Error) -> DbError {
    DbError::Io {
        path: path.display().to_string(),
        kind: e.kind(),
        message: e.to_string(),
    }
}

fn csv_err(path: &Path, e: csv::Error) -> DbError {
    // csv::Error 是个结构体,具体原因得从 kind() 里取;ErrorKind 本身没有
    // Display,所以兜底分支用 e 自己的描述.
    match e.kind() {
        csv::ErrorKind::Io(io) => DbError::Io {
            path: path.display().to_string(),
            kind: io.kind(),
            message: io.to_string(),
        },
        csv::ErrorKind::UnequalLengths {
            pos,
            expected_len,
            len,
        } => DbError::Format {
            path: path.display().to_string(),
            line: pos.as_ref().map(|p| p.line() as usize),
            message: format!("expected {} fields, found {}", expected_len, len),
        },
        csv::ErrorKind::Utf8 { pos, err } => DbError::Format {
            path: path.display().to_string(),
            line: pos.as_ref().map(|p| p.line() as usize),
            message: format!("invalid UTF-8: {}", err),
        },
        _ => DbError::Format {
            path: path.display().to_string(),
            line: None,
            message: e.to_string(),
        },
    }
}
