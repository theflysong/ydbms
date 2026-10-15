//! 元命令的解析.
//!
//! 只做**语法**:把一行拆成命令名与参数.所有校验(路径存在吗,参数给全了吗,
//! 变量认不认识)都留给执行,因为执行那边才有输出去向可以报告错误.认不出来的
//! 命令也照样返回 [`MetaCmd::Unknown`],让它走到执行层去打印"未知元命令".
//!
//! 与 psql 一致:元命令只在**行首**(trim 之后)才算,参数取到行尾,所以路径
//! (以及 `\c` 的库名)可以带空格.

use std::path::PathBuf;

/// 所有命令名(含 `\quit` 这种别名).
///
/// Tab 补全与"未知元命令"的提示共用这一份,**不许两处各写一遍**--加一条命令
/// 忘了补另一处,用户就会看到自相矛盾的提示.
pub const NAMES: &[&str] = &[
    "\\q", "\\quit", "\\!", "\\i", "\\pwd", "\\ls", "\\cd", "\\echo", "\\dt", "\\d", "\\o",
    "\\timing", "\\set", "\\l", "\\du", "\\c", "\\connect",
];

/// 一条元命令.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaCmd {
    /// `\q` -- 退出.
    Quit,
    /// `\! <命令>` -- 执行 shell 命令.**脚本里会被拒绝**.
    Shell(String),
    /// `\i <路径>` -- 读脚本.
    Include(Option<PathBuf>),
    /// `\pwd` -- 当前目录.
    Pwd,
    /// `\ls [目录]` -- 列目录.
    Ls(Option<PathBuf>),
    /// `\cd [目录]` -- 切换目录;无参回 `$HOME`.
    Cd(Option<PathBuf>),
    /// `\echo <文本>` -- 原样回显(整行被一对单引号包住时由执行层去掉引号).
    Echo(String),
    /// `\dt` -- 列出所有表.
    ListTables,
    /// `\d [表名]` -- 无参等同于 `\dt`;有参列出该表的列.
    Describe(Option<String>),
    /// `\o [文件]` -- 重定向**查询结果表**的输出;无参回标准输出.
    Output(Option<PathBuf>),
    /// `\timing [on|off]` -- 每条语句后打印耗时;无参**切换**(与 psql 一致).
    ///
    /// 装的是**原样的取值**(可能是空串):语法层不解释它,`yes` 这种认不出来的
    /// 写法留给执行层去报错--别在这里悄悄当成开关的哪一边.
    Timing(String),
    /// `\l` -- 列出服务目录里的库与 owner.
    ListDatabases,
    /// `\du` -- 列出角色(名字,属性,所属角色).
    ListRoles,
    /// `\c [库]` / `\connect [库]` -- 换一个库.**脚本里也能用**.
    ///
    /// 参数取到行尾(与 `\i` / `\o` 一样),所以库名里可以有空格.
    Connect(Option<String>),
    /// `\set <变量> [值]` -- 这里只拆词;认不认识这个变量由执行层判断.
    Set {
        /// 变量名.
        name: String,
        /// 变量取值;没写就是 `None`.
        value: Option<String>,
    },
    /// 认不出来的元命令.
    Unknown(String),
}

/// 这一行是元命令吗?是就解析出来.
pub fn parse(line: &str) -> Option<MetaCmd> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('\\')?;

    let (name, rest) = match body.find(char::is_whitespace) {
        Some(index) => (&body[..index], body[index..].trim()),
        None => (body, ""),
    };

    // 参数:空就是 None(`\i` 与 `\i ` 都算没给参数)
    let path = || {
        if rest.is_empty() {
            None
        } else {
            Some(PathBuf::from(rest))
        }
    };

    Some(match name {
        "q" | "quit" => MetaCmd::Quit,
        "!" => MetaCmd::Shell(rest.to_string()),
        "i" => MetaCmd::Include(path()),
        "pwd" => MetaCmd::Pwd,
        "ls" => MetaCmd::Ls(path()),
        "cd" => MetaCmd::Cd(path()),
        "echo" => MetaCmd::Echo(rest.to_string()),
        "dt" => MetaCmd::ListTables,
        "d" => MetaCmd::Describe(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        "o" => MetaCmd::Output(path()),
        "l" => MetaCmd::ListDatabases,
        "du" => MetaCmd::ListRoles,
        "c" | "connect" => MetaCmd::Connect(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        "timing" => MetaCmd::Timing(rest.to_string()),
        "set" => {
            let (name, value) = match rest.find(char::is_whitespace) {
                Some(index) => (rest[..index].to_string(), rest[index..].trim()),
                None => (rest.to_string(), ""),
            };
            MetaCmd::Set {
                name,
                value: if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                },
            }
        }
        other => MetaCmd::Unknown(format!("\\{}", other)),
    })
}
