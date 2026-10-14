//! 用「标准 CSV + 旁路 JSON 模式文件」实现的持久化。
//!
//! 数据用 [`csv`] crate 读写（引号、逗号、换行等规则都由它处理），类型信息写成
//! 同目录下的 `<name>.schema.json`。布局与一致性约定见 [`crate::pers`]。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Persister, TablePath};
use crate::db::phys::PhysTab;
use crate::db::table::{
    Column, ColumnsShape, DbError, Result, Row, Table, TabShape, Value,
};

/// 模式文件的格式版本。
///
/// `load` 遇到不认识的版本会直接报错，而不是按当前格式硬读——宁可失败也不静默
/// 读错数据。将来改格式时靠它做迁移。
const SCHEMA_VERSION: u32 = 1;

/// 旁路模式文件的内容。
#[derive(Debug, Serialize, Deserialize)]
struct SchemaFile {
    /// 格式版本，见 [`SCHEMA_VERSION`]。
    version: u32,
    /// 表的逻辑名。
    name: String,
    /// 列定义，顺序即 CSV 的列序。
    columns: Vec<Column>,
}

/// 以 CSV 存放数据、以旁路 JSON 存放模式的 [`Persister`]。
#[derive(Debug, Clone, Copy, Default)]
pub struct CsvPersister;

impl CsvPersister {
    /// 新建一个实现。它是无状态的，等价于 [`CsvPersister::default`]。
    pub fn new() -> Self {
        CsvPersister
    }
}

impl Persister for CsvPersister {
    fn save(&self, table: &dyn Table, at: &TablePath) -> Result<()> {
        let data_path = at.data_path();
        let schema_path = at.schema_path();

        if let Some(dir) = data_path.parent() {
            fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }

        // 先写临时文件再改名：读者不会看到一个写了一半的文件。
        // 临时文件必须和目标同目录，rename 才是同文件系统内的原子操作。
        let data_tmp = tmp_path(data_path);
        if let Err(e) = write_csv(&data_tmp, table) {
            remove_quietly(&data_tmp);
            return Err(e);
        }
        fs::rename(&data_tmp, data_path).map_err(|e| io_err(data_path, e))?;

        let schema_tmp = tmp_path(schema_path);
        if let Err(e) = write_schema(&schema_tmp, table, schema_path) {
            remove_quietly(&schema_tmp);
            return Err(e);
        }
        fs::rename(&schema_tmp, schema_path).map_err(|e| io_err(schema_path, e))?;

        Ok(())
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

        // 3) 交叉校验：两份文件必须描述同一张表，否则宁可报错也不读错。
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

        // 4) 逐行还原。表名以模式文件为准。
        let mut table = PhysTab::new(schema.name, &shape);
        for record in reader.records() {
            let record = record.map_err(|e| csv_err(data_path, e))?;
            let line = record.position().map(|p| p.line() as usize);

            let mut row: Row = Vec::with_capacity(shape.arity());
            for (i, col) in shape.columns().iter().enumerate() {
                // flexible(false) 已经挡住字段数不齐，这里只是防御。
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

/// 把表写成 CSV：先写一条纯列名表头，再逐行写数据。
fn write_csv(path: &Path, table: &dyn Table) -> Result<()> {
    let mut writer = csv::Writer::from_path(path).map_err(|e| csv_err(path, e))?;

    writer
        .write_record(table.columns().iter().map(|c| c.name.as_str()))
        .map_err(|e| csv_err(path, e))?;

    for row in table.iter_rows() {
        // 挡住实现有问题的表，别把坏数据写进文件。
        table.validate(&row)?;
        writer
            .write_record(row.iter().map(|v| v.to_csv_field()))
            .map_err(|e| csv_err(path, e))?;
    }

    // Writer::flush 返回的是 io::Result，不是 csv::Error。
    writer.flush().map_err(|e| io_err(path, e))?;
    Ok(())
}

/// 把模式写成 JSON。
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

/// 按列类型把 CSV 的一个字段解析成 [`Value`]。
///
/// 解析是**严格**的：不接受前后空白，`" 5"` 会被当作坏数据。
fn parse_field(field: &str, column: &Column, path: &Path, line: Option<usize>) -> Result<Value> {
    // 类型怎么解析、typmod 怎么强制，全在 `DataType::parse` 里；这里只负责
    // 把失败包装成「文件内容有问题」的 Format 错误。
    column.ty.from_csv_field(field).map_err(|e| DbError::Format {
        path: path.display().to_string(),
        line,
        message: format!("column `{}`: {}", column.name, e),
    })
}

/// `f.csv` → `f.csv.tmp`（同目录，保证 rename 是原子的）。
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
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
    // csv::Error 是个结构体，具体原因得从 kind() 里取；ErrorKind 本身没有
    // Display，所以兜底分支用 e 自己的描述。
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
