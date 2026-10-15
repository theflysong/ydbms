//! Tab 补全的候选:**纯逻辑**,不认识终端.
//!
//! rustyline 只负责"按 Tab 时叫我们,拿到候选自己决定怎么写进去";"这个上下文该补
//! 什么"全在这里,所以可以直接单测(见 `tests/repl.rs`).要补的东西看光标前的那个词:
//!
//! | 光标前是 | 补 |
//! |---|---|
//! | `\` 开头,还在敲命令名 | 元命令([`crate::cmd::metacmd::NAMES`],唯一一份) |
//! | `\d ` | 表名 |
//! | `\i ` `\o ` `\ls ` `\cd ` | 文件路径(目录补 `/`) |
//! | `\timing ` `\set ON_ERROR_STOP ` `\set VERBOSE ` | `on` / `off` |
//! | `\set ` | 变量名(本版有 `ON_ERROR_STOP` 与 `VERBOSE`) |
//! | SQL | 关键字 ∪ 表名 ∪ 列名;`别名.` 之后只补那个表的列 |
//!
//! 前缀匹配**不分大小写**(`sel` 能补出 `SELECT`),写进去的关键字统一是大写.
//!
//! # 不够聪明的地方(有意)
//!
//! 不解析 SQL 语法:`FROM ` 后面和 `SELECT ` 后面给的是同一堆候选.真正的"按上下文
//! 只给该给的"要等绑定器有作用域信息,那与补全不是一回事.

use std::path::PathBuf;

use crate::ir::SymbolTable;

use super::metacmd;

/// SQL 关键字候选.**只列引擎真正支持的词**--补出一个本版不认的写法,比不补更糟.
const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER", "LIMIT", "OFFSET", "JOIN",
    "INNER", "LEFT", "OUTER", "ON", "AS", "AND", "OR", "NOT", "NULL", "IS", "CASE", "WHEN",
    "THEN", "ELSE", "END", "CREATE", "TABLE", "IF", "EXISTS", "INSERT", "INTO", "VALUES", "ASC",
    "DESC", "NULLS", "FIRST", "LAST", "DISTINCT", "COUNT", "SUM", "AVG", "MIN", "MAX", "ROUND",
    "TRUE", "FALSE", "INT", "INTEGER", "TEXT", "VARCHAR", "CHAR", "TIMESTAMP", "DATE",
    "NUMERIC", "LENGTH", "UPDATE", "SET", "CHECK", "DEFAULT",
];

/// `on` / `off` 这类开关字的候选.
const SWITCHES: &[&str] = &["on", "off"];

/// 本版认的 `\set` 变量(与 [`crate::cmd::engine`] 的判断一致).
const VARIABLES: &[&str] = &["ON_ERROR_STOP", "VERBOSE"];

/// 补全的候选来源:现在有哪些表,哪些列,服务目录里有哪些库.
///
/// 由 [`Catalog::from_symbols`] 从符号表现算--上一条语句刚建出来的表,下一条就该能补出来;
/// `databases` 由会话那一层填(内存模式留空),只有 `\c` 用得上.
pub struct Catalog {
    /// 表名(含别名与 `study.movies` 这类限定名,照符号表里的原样).
    pub tables: Vec<String>,
    /// 所有列名(跨表去重).
    pub columns: Vec<String>,
    /// 服务目录里的库名(内存模式为空).
    pub databases: Vec<String>,
    /// 每张表自己的列,给 `别名.` 这种限定补全用.
    per_table: Vec<(String, Vec<String>)>,
}

impl Catalog {
    /// 从符号表算一份补全用的目录.
    pub fn from_symbols(symbols: &SymbolTable) -> Catalog {
        let mut per_table = Vec::new();
        let mut columns: Vec<String> = Vec::new();

        for name in symbols.names() {
            let Some(found) = symbols.columns(name) else {
                continue;
            };
            let names: Vec<String> = found.iter().map(|c| c.name.clone()).collect();
            for column in &names {
                if !columns.contains(column) {
                    columns.push(column.clone());
                }
            }
            per_table.push((name.to_string(), names));
        }

        let mut tables: Vec<String> = symbols.names().iter().map(|n| n.to_string()).collect();
        tables.sort();
        columns.sort();

        Catalog {
            tables,
            columns,
            databases: Vec::new(),
            per_table,
        }
    }

    /// `qualifier` 指向哪张表?按整名或最后一段(`study.movies` → `movies`)认.
    fn columns_of(&self, qualifier: &str) -> Option<&[String]> {
        self.per_table
            .iter()
            .find(|(name, _)| name == qualifier || name.rsplit('.').next() == Some(qualifier))
            .map(|(_, columns)| columns.as_slice())
    }
}

/// 一个候选:`insert` 写进缓冲区,`show` 是列出来时显示的名字.
///
/// 大多数时候两者一样;文件路径不一样--写进去的是全路径,列出来的只是条目名.
pub struct Candidate {
    /// 写进缓冲区的文本.
    pub insert: String,
    /// 列出来时显示的名字.
    pub show: String,
}

impl Candidate {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Candidate {
            show: text.clone(),
            insert: text,
        }
    }
}

/// 一次补全请求算出来的东西.
pub struct Suggestions {
    /// 要替换的区间起点(**字节**下标,落在字符边界上).
    pub start: usize,
    /// 区间终点(字节下标,就是光标处).
    pub end: usize,
    /// 候选词.
    pub items: Vec<Candidate>,
}

/// 算一次补全.`cursor` 与结果里的 `start`/`end` 都是**字节**下标(rustyline 的约定).
///
/// `None` 表示这里没什么可补的.
pub fn suggest(line: &str, cursor: usize, catalog: &Catalog) -> Option<Suggestions> {
    let cursor = floor_char_boundary(line, cursor);
    let head = &line[..cursor];

    if head.trim_start().starts_with('\\') {
        return metacommand(head, cursor, catalog);
    }

    // 空行按 Tab 不给候选:省得刷一屏关键字
    if head.trim().is_empty() {
        return None;
    }

    let start = ident_start(head);
    let word = &head[start..];
    let (qualifier, prefix) = match word.split_once('.') {
        Some((qualifier, rest)) => (Some(qualifier), rest),
        None => (None, word),
    };

    let items = match qualifier {
        // `别名.` 之后只补那张表的列
        Some(qualifier) => catalog
            .columns_of(qualifier)
            .map(|columns| matched(columns, prefix, |column| format!("{}.{}", qualifier, column)))
            .unwrap_or_default(),
        None => {
            let mut items = matched(KEYWORDS, prefix, str::to_string);
            items.extend(matched(&catalog.tables, prefix, str::to_string));
            items.extend(matched(&catalog.columns, prefix, str::to_string));
            items
        }
    };

    finish(items, start, cursor)
}

/// 元命令行:先补命令名,再按命令名补参数.
fn metacommand(head: &str, cursor: usize, catalog: &Catalog) -> Option<Suggestions> {
    // 行首空白之后就是 `\` 开头的那一段
    let body_start = head.len() - head.trim_start().len();
    let body = &head[body_start..];
    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = &body[..name_end];

    // 还在敲命令名本身:把 `\xx` 整个换掉
    if name_end == body.len() {
        let items = NAMES
            .iter()
            .filter(|candidate| candidate.starts_with(name))
            .map(|candidate| Candidate::new(*candidate))
            .collect();
        return finish(items, body_start, cursor);
    }

    let start = last_word_start(head);
    let word = &head[start..];

    let items = match name {
        "\\i" | "\\o" | "\\ls" | "\\cd" => paths(word),
        "\\d" => matched(&catalog.tables, word, str::to_string),
        "\\c" | "\\connect" => matched(&catalog.databases, word, str::to_string),
        "\\timing" => matched(SWITCHES, word, str::to_string),
        // `\set`:先补变量名,变量名写完再补取值
        "\\set" => {
            let args = body[name_end..].trim_start();
            if args.find(char::is_whitespace).is_some() {
                matched(SWITCHES, word, str::to_string)
            } else {
                matched(VARIABLES, word, str::to_string)
            }
        }
        _ => return None,
    };

    finish(items, start, cursor)
}

/// 前缀(不分大小写)过滤 + 转成候选.`render` 决定写进去的文本长什么样.
fn matched<T, F>(pool: &[T], prefix: &str, render: F) -> Vec<Candidate>
where
    T: AsRef<str>,
    F: Fn(&str) -> String,
{
    pool.iter()
        .filter(|candidate| starts_with_ignore_case(candidate.as_ref(), prefix))
        .map(|candidate| Candidate::new(render(candidate.as_ref())))
        .collect()
}

/// 文件路径补全:`word` 是已经打出来的那一段(可能带目录).
///
/// 目录本身补一个 `/`;读不了的目录就当没有候选,不报错.
fn paths(word: &str) -> Vec<Candidate> {
    let (dir_text, name_prefix) = match word.rfind('/') {
        Some(index) => (&word[..=index], &word[index + 1..]),
        None => ("", word),
    };
    let dir = if dir_text.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(dir_text)
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut items: Vec<Candidate> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(name_prefix) {
                return None;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let insert = format!("{}{}{}", dir_text, name, if is_dir { "/" } else { "" });
            Some(Candidate { insert, show: name })
        })
        .collect();

    items.sort_by(|a, b| a.insert.cmp(&b.insert));
    items
}

/// 候选为空就是"没什么可补的".
fn finish(items: Vec<Candidate>, start: usize, end: usize) -> Option<Suggestions> {
    if items.is_empty() {
        None
    } else {
        Some(Suggestions { start, end, items })
    }
}

/// 不分大小写的前缀匹配.
fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// 把下标夹到**字符边界**上:rustyline 给的是字节位置,可能落在多字节字符中间,
/// 直接切会 panic.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 光标前那个标识符的起点.`.` 也算在里面:`t.col` 要整个换掉,不能只换 `col`.
fn ident_start(head: &str) -> usize {
    head.char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '.'))
        .map_or(0, |(index, c)| index + c.len_utf8())
}

/// 最后一个**空白分隔**的词的起点.路径里有 `/`,不能按标识符字符切.
fn last_word_start(head: &str) -> usize {
    head.char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8())
}

/// 元命令名的候选表(就在 `metacmd.rs` 里,只此一份).
const NAMES: &[&str] = metacmd::NAMES;
