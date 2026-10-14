//! 语句层：谁产出结果集，谁有副作用。
//!
//! [`Plan`] 是**纯查询 IR**——它描述"算出哪些行"，`exec::execute` 的签名是
//! `(&Plan, &SymbolTable) -> PhysTab`，只读、无副作用。它内部是**拉取式**的
//! （`exec::execute_source` 造管道、`execute` 抽干 root），但这里只关心"结果表"。DDL/DML 不是这种东西：
//! 它们要改符号表（建表）或改表内容（插数据）。所以它们放在这一层，而不是往
//! `Op` 里塞一个 `Insert` 变体——那样会让 `execute` 变成 `&mut`，把"改符号表"
//! 这件事污染进整条查询链路和所有算子。
//!
//! 表达式仍然复用线性 IR：`INSERT` 里的值就是 [`Prog`]，白拿 `Prog::verify`
//! 与 `Prog::eval`。
//!
//! # 为什么必须逐条「降级 → 执行」
//!
//! 后面的语句依赖前面建出来的表：`INSERT INTO prime` 的降级需要 `prime` 已经
//! 在符号表里，而它是上一条 `CREATE TABLE` 刚建的。所以 [`run_script`] 不能
//! 先把整个脚本降级完，只能一条一条来。
//!
//! # 为什么还提供 [`Script`]
//!
//! [`run_script`] 一口气跑完，调用方拿不到"每隔一条"的粒度——而 `\timing`
//! 要量每条语句的耗时、`ON_ERROR_STOP` 要在第一条错误处停下，两者都要求
//! **由调用方驱动循环**。所以把"解析"和"逐条执行"拆开：[`Script::parse`] 解析
//! 一次，[`Script::run_one`] 跑一条（降级仍在执行时做，见上）。`run_script`
//! 就是这两步的循环，签名与行为一个字没变。
//!
//! [`has_top_level_terminator`] 是给 REPL 用的：判断"这段输入里有没有一句说完"，
//! 于是多行 SQL 能攒到分号才提交，而字符串/注释里的分号不会被误切。

use pg_query::protobuf::RawStmt;

use crate::db::phys::PhysTab;
use crate::db::table::{Column, ColumnsShape, DbError, Result, Row};

use super::expr::Prog;
use super::plan::Plan;
use super::symbol::SymbolTable;
use super::{exec, lower};

/// 一条语句。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// 查询：产出一张结果表。纯的、无副作用。
    Query(Plan),

    /// `CREATE TABLE`：在符号表里登记一张空表。
    CreateTable {
        /// 表名（可能带 schema，如 `study.people`）。
        name: String,
        /// 列定义。
        columns: Vec<Column>,
        /// 语句里写的是 `CREATE TABLE IF NOT EXISTS`。
        if_not_exists: bool,
        /// **接受但没有强制**的约束，例如 `"PRIMARY KEY (prime_id)"`。
        ///
        /// 这个字段存在的意义就是"不许偷偷不强制"：调用方要把它显示出来。
        /// `NOT NULL` 不在其中——它是**真的强制**的（`Column::not_null`，
        /// 由 `Table::validate` 在 INSERT 时拦住）。
        unenforced: Vec<String>,
    },

    /// `INSERT ... VALUES`：往表里加行。
    Insert {
        /// 表名。
        table: String,
        /// 每一行按**表的列序**给出全部列的值。
        ///
        /// 降级时已经按语句里的列名表重排过了，所以这里的位置是确定的；值表达式
        /// 没有输入行，作用域为空，出现列引用会被 `Prog::verify` 挡下。
        rows: Vec<Vec<Prog>>,
    },
}

/// 一条语句执行完的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 查询的结果表。
    Rows(PhysTab),
    /// 建表完成。
    Created {
        /// 表名。
        name: String,
        /// 见 [`Stmt::CreateTable::unenforced`]。
        unenforced: Vec<String>,
    },
    /// 插入完成。
    Inserted {
        /// 表名。
        table: String,
        /// 实际插入的行数。
        rows: usize,
    },
}

/// 一条语句的执行记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StmtResult {
    /// 在脚本里的序号（从 1 开始）。
    pub index: usize,
    /// 语句原文，已压成一行、已截断。
    pub text: String,
    /// 这条语句的成败。
    pub outcome: Result<Outcome>,
}

/// 在符号表上执行一条语句。
///
/// 只有 [`Stmt::CreateTable`] 与 [`Stmt::Insert`] 会改东西：前者往符号表里加表，
/// 后者改表内容。语句自己的失败不会留下半拉状态——建表是"要么登记要么报错"，
/// 插入是逐行过 [`Table::insert`](crate::db::table::Table::insert) 的校验。
pub fn execute_stmt(stmt: &Stmt, symbols: &mut SymbolTable) -> Result<Outcome> {
    match stmt {
        Stmt::Query(plan) => Ok(Outcome::Rows(exec::execute(plan, symbols)?)),

        Stmt::CreateTable {
            name,
            columns,
            if_not_exists,
            unenforced,
        } => {
            if symbols.lookup(name).is_some() {
                if *if_not_exists {
                    // `IF NOT EXISTS` 且已经有了：视为满足要求，不报错。
                    return Ok(Outcome::Created {
                        name: name.clone(),
                        unenforced: unenforced.clone(),
                    });
                }
                return Err(DbError::DuplicateSymbol { name: name.clone() });
            }

            let shape = ColumnsShape::new(columns.clone());
            symbols.define(name.clone(), PhysTab::new(name.clone(), &shape))?;

            Ok(Outcome::Created {
                name: name.clone(),
                unenforced: unenforced.clone(),
            })
        }

        Stmt::Insert { table, rows } => {
            let target = symbols
                .relation_mut(table)
                .ok_or_else(|| DbError::UnknownSymbol {
                    name: table.clone(),
                })?;

            let mut inserted = 0usize;
            for programs in rows {
                let mut row = Row::with_capacity(programs.len());
                for prog in programs {
                    // 值表达式没有输入行。
                    let scalar = prog.eval(&[]).map_err(|e| e.into_db_error(None))?;
                    let value = scalar.to_value().ok_or_else(|| DbError::IrRuntime {
                        node: None,
                        message: format!("要插入的值必须是 INT，实际是 {}", scalar),
                    })?;
                    row.push(value);
                }
                // 走 Table::insert，于是元数与类型校验都被复用。
                target.insert(row)?;
                inserted += 1;
            }

            Ok(Outcome::Inserted {
                table: table.clone(),
                rows: inserted,
            })
        }
    }
}

/// 解析整段 SQL（可含多条语句），逐条「降级 → 执行」。
///
/// 外层 `Result` 只表示"解析/切分"这一步就失败了（那时一条都没跑）；每条语句
/// 自己的成败在 [`StmtResult::outcome`] 里。**一条失败不影响后面的语句**——
/// 与 psql 默认行为一致。
///
/// 注意：**没有事务、不回滚**。前面成功的语句已经生效，脚本中途失败会留下一个
/// 建了一半的库。
///
/// 想要"一条一条自己驱动"（计时、遇错就停）用 [`Script`]。
pub fn run_script(sql: &str, symbols: &mut SymbolTable) -> Result<Vec<StmtResult>> {
    let script = Script::parse(sql)?;
    Ok((0..script.len())
        .map(|index| script.run_one(index, symbols))
        .collect())
}

/// 一份**已解析**的脚本：可以逐条降级 + 执行。
///
/// 解析与执行分开，是为了让调用方拿到"每条语句"的粒度：`\timing` 要在一条语句
/// 前后计时，`ON_ERROR_STOP` 要在第一条失败处停下。**降级仍然在每条执行时才做**
/// （后面的语句依赖前面建出来的表）。
///
/// ```
/// use ydbms::ir::{Script, SymbolTable};
///
/// let mut symbols = SymbolTable::new();
/// let script = Script::parse("CREATE TABLE t (a INT); INSERT INTO t VALUES (1);").unwrap();
/// assert_eq!(script.len(), 2);
/// let created = script.run_one(0, &mut symbols);
/// assert!(created.outcome.is_ok());
/// let inserted = script.run_one(1, &mut symbols);
/// assert!(inserted.outcome.is_ok());
/// ```
pub struct Script {
    /// 原文：`statement_text` 要从里面切出每条语句的文本。
    sql: String,
    /// `pg_query` 的裸语句节点。**保持私有**：`pg_query` 的类型不外泄。
    statements: Vec<RawStmt>,
}

impl Script {
    /// 解析整段 SQL。这里的 `Err` 表示**一条都没跑**。
    pub fn parse(sql: &str) -> Result<Script> {
        let parsed = pg_query::parse(sql).map_err(|e| DbError::Parse {
            message: e.to_string(),
        })?;

        Ok(Script {
            sql: sql.to_string(),
            statements: parsed.protobuf.stmts,
        })
    }

    /// 语句条数。
    pub fn len(&self) -> usize {
        self.statements.len()
    }

    /// 一条语句都没有。
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// 执行第 `index` 条（0 起），返回这条的记录。
    ///
    /// 调用方按 `0..len()` 循环，所以下标一定在界内。**失败不中止**后续语句——
    /// 要不要停由调用方决定（`ON_ERROR_STOP`）。
    pub fn run_one(&self, index: usize, symbols: &mut SymbolTable) -> StmtResult {
        let raw = &self.statements[index];

        // 逐条来：后面的语句依赖前面建出来的表，不能先全降级。
        let outcome = match raw.stmt.as_deref() {
            Some(node) => match lower::lower_statement(node, symbols) {
                Ok(stmt) => execute_stmt(&stmt, symbols),
                Err(e) => Err(e),
            },
            None => Err(DbError::Parse {
                message: "空的语句".to_string(),
            }),
        };

        StmtResult {
            index: index + 1,
            text: statement_text(&self.sql, raw),
            outcome,
        }
    }
}

/// 这段 SQL 里有没有**顶层**的分号——REPL 用它判断"这一句敲完了没有"。
///
/// "顶层" = 不在字符串、注释或美元引用里：
///
/// * `'...'`（`''` 是一个字面单引号）与 `"..."`（`""` 同理）；
/// * `-- ...` 直到行尾；`/* ... */`，**允许嵌套**（PostgreSQL 也允许）；
/// * `$$...$$` 与 `$tag$...$tag$`。
///
/// 已知不处理：`E'...'` 里的反斜杠转义。PG 默认 `standard_conforming_strings = on`，
/// 普通 `'...'` 里的反斜杠是字面量，所以只有显式写了 `E''` 才会看错。
pub fn has_top_level_terminator(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b';' => return true,
            quote @ (b'\'' | b'"') => i = skip_quoted(bytes, i, quote),
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line_comment(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            b'$' => i = skip_dollar_quoted(bytes, i),
            // ASCII 之外都是 UTF-8 的续字节，不会与上面的字节撞上
            _ => i += 1,
        }
    }

    false
}

/// 跳过一段 `'...'` / `"..."`，返回它后面那个字节的位置。没闭合就到结尾。
fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            // 两个引号连写 = 一个字面引号，不算结束
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// 跳过 `-- ...`（含行尾换行）。
fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    match bytes[start..].iter().position(|b| *b == b'\n') {
        Some(offset) => start + offset + 1,
        None => bytes.len(),
    }
}

/// 跳过 `/* ... */`，**按嵌套算**（`/* a /* b */ c */` 是一整段注释）。
fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;

    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }

    bytes.len()
}

/// 跳过 `$$...$$` / `$tag$...$tag$`。不是合法的美元引用就只前进一个字节
/// （`$1` 之类的参数占位符不该把它吃掉）。
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> usize {
    let tag_end = match bytes[start + 1..].iter().position(|b| *b == b'$') {
        Some(offset) => start + 1 + offset,
        None => return start + 1,
    };

    // 标签只能是标识符字符（`$$` 的标签是空的）
    let is_tag = bytes[start + 1..tag_end]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if !is_tag {
        return start + 1;
    }

    let tag = &bytes[start..=tag_end];
    let mut i = tag_end + 1;
    while i + tag.len() <= bytes.len() {
        if &bytes[i..i + tag.len()] == tag {
            return i + tag.len();
        }
        i += 1;
    }

    bytes.len()
}

/// 从原脚本里切出这条语句的原文。
///
/// 用 [`RawStmt`] 的 `stmt_location` / `stmt_len`。实测这两个字段是准的，但
/// 长度的口径会把尾随换行算进去，所以一律 `trim`；切片越界就退回"从头切到
/// 结尾"或空串，绝不 panic。
fn statement_text(sql: &str, raw: &RawStmt) -> String {
    let start = raw.stmt_location.max(0) as usize;
    let len = raw.stmt_len.max(0) as usize;

    let slice = if len == 0 {
        sql.get(start..)
    } else {
        sql.get(start..start + len).or_else(|| sql.get(start..))
    };

    // `stmt_location` 把**前导注释**也算进语句里，所以带注释的脚本里这个切片
    // 会以 `-- ...` 开头，`[N] <原文>` 就显示成注释而不是 SQL。先剥掉。
    let body = strip_leading_comments(slice.unwrap_or(""));

    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");

    truncate(&one_line, 60)
}

/// 剥掉开头的空白与 SQL 注释（`--` 行注释与 `/* */` 块注释）。
fn strip_leading_comments(text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            match after.find('\n') {
                Some(nl) => rest = &after[nl + 1..],
                None => return "",
            }
        } else if let Some(after) = rest.strip_prefix("/*") {
            match after.find("*/") {
                Some(end) => rest = &after[end + 2..],
                None => return "",
            }
        } else {
            return rest;
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}
