//! 交互式终端：把敲进来的东西交给引擎。
//!
//! 终端只做两件引擎管不了的事：**行编辑**（rustyline：Tab 补全、历史、光标）与
//! **"这一句够了吗"**（[`is_complete`]）。剩下的——SQL、元命令、两路输出——都在
//! [`Session`] 里，交互输入与 `\i` 读进来的内容在引擎看来是同一件事。
//!
//! # 多行怎么判断
//!
//! rustyline 的 `Validator` 每次回车都问一次"够了吗"：不够就继续收行（这就是
//! "累积到分号才执行"），够了才把**整段**交出去。规则见 [`is_complete`]。
//!
//! # 非 tty 怎么办
//!
//! 管道/重定向没有终端可编辑，走 `run_plain`：逐行读、同样按 [`is_complete`]
//! 累积，**EOF 时把攒着的交出去**。于是 `printf 'SELECT 1\n' | ydbms` 照旧能用。
//!
//! # `TERM` 的讲究
//!
//! rustyline 在 `TERM` 是 `dumb` / `cons25` / `emacs` 时**主动降级**（不接手行编辑，
//! 也就没有补全）。这是它的既定行为，与 readline 在哑终端上的表现一致。

use std::io::{self, BufRead, IsTerminal};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{CompletionType, Config, Context, Editor, Helper, Result};

use crate::ir;

use super::complete::{self, Catalog};
use super::engine::Session;

/// 交互式终端入口：打横幅、读输入、交给引擎，`\q`（或 Ctrl-D）退出。
pub fn run() -> io::Result<()> {
    let mut session = Session::new();
    banner();

    if !io::stdin().is_terminal() {
        return run_plain(&mut session);
    }

    let config = Config::builder()
        .max_history_size(200)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?
        .completion_type(CompletionType::List)
        .build();

    let mut editor = Editor::<Repl, DefaultHistory>::with_config(config)
        .map_err(as_io_error)?;
    editor.set_helper(Some(Repl {
        catalog: session.catalog(),
    }));

    loop {
        // 每次读之前刷新候选：上一条语句刚建出来的表，这一条就该能补出来
        if let Some(helper) = editor.helper_mut() {
            helper.catalog = session.catalog();
        }

        match editor.readline("sql> ") {
            Ok(line) => {
                let _ = editor.add_history_entry(line.as_str());
                submit(&mut session, &line);
                if session.should_quit() {
                    break;
                }
            }
            // Ctrl-C：把这一行丢掉，重新来
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl-D：退出
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(as_io_error(e)),
        }
    }

    println!("再见。");
    Ok(())
}

/// rustyline 的错误转成 [`io::Error`]：`ReadlineError` 不能直接 `?` 进 `io::Result`。
fn as_io_error(e: ReadlineError) -> io::Error {
    io::Error::other(e.to_string())
}

/// 没有终端可编辑时的回退：逐行读，按 [`is_complete`] 累积后再交出去。
///
/// 这一路就是"把脚本喂进来"（`ydbms < s.sql`），所以 **`ON_ERROR_STOP` 命中后不再读
/// 后面的语句**——与 psql 一致（实测：psql 从管道喂带 `ON_ERROR_STOP` 的脚本，
/// 后面那些语句一条都不跑）。
fn run_plain(session: &mut Session) -> io::Result<()> {
    let stdin = io::stdin();
    let mut pending = String::new();

    for line in stdin.lock().lines() {
        let line = line?;
        pending.push_str(&line);
        pending.push('\n');

        if !is_complete(&pending) {
            continue;
        }
        submit(session, &pending);
        pending.clear();
        if session.should_quit() || session.stopped() {
            return Ok(());
        }
    }

    // EOF：攒着的也交出去（`SELECT 1` 不打分号也要能跑）
    if !pending.trim().is_empty() {
        submit(session, &pending);
    }
    Ok(())
}

/// 一段输入怎么交给引擎：`exit` 当作 `\q`，`EXPLAIN` 走"只降级不执行"那条路。
fn submit(session: &mut Session, input: &str) {
    if input.trim().eq_ignore_ascii_case("exit") {
        session.run_interactive("\\q");
        return;
    }

    match strip_command(input.trim_start(), "EXPLAIN") {
        Some(sql) if !sql.trim().is_empty() => session.explain(sql),
        _ => session.run_interactive(input),
    }
}

/// REPL 的"这一句够了吗"。够一句的四种情况：
///
/// 1. 去掉空白之后是空的（回车什么也不干）；
/// 2. **最后一个非空行**以 `\` 开头——元命令一行就是一句（引擎的 `split` 会先把
///    挂着的 SQL 跑掉再跑它，顺序仍然对）；
/// 3. 第一行以 `EXPLAIN` 开头——交互终端的老手感：`EXPLAIN SELECT …` 不用打分号；
/// 4. SQL 里出现了**顶层分号**（不在字符串、注释、`$$` 里），见
///    [`ir::has_top_level_terminator`]。
pub fn is_complete(input: &str) -> bool {
    if input.trim().is_empty() {
        return true;
    }

    if let Some(last) = input.lines().rev().find(|line| !line.trim().is_empty()) {
        if last.trim_start().starts_with('\\') {
            return true;
        }
    }

    if let Some(first) = input.lines().find(|line| !line.trim().is_empty()) {
        if strip_command(first.trim_start(), "EXPLAIN").is_some() {
            return true;
        }
    }

    ir::has_top_level_terminator(input)
}

/// rustyline 的接线：补全交给 [`complete::suggest`]，多行判定交给 [`is_complete`]。
struct Repl {
    /// 补全用的目录；每读一行之前刷新一次。
    catalog: Catalog,
}

impl Helper for Repl {}

// 不用提示（`rustyline` 要求实现这两个 trait）
impl Hinter for Repl {
    type Hint = String;
}

// 不做语法高亮
impl Highlighter for Repl {}

impl Completer for Repl {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Result<(usize, Vec<Pair>)> {
        Ok(match complete::suggest(line, pos, &self.catalog) {
            Some(found) => (
                found.start,
                found
                    .items
                    .into_iter()
                    .map(|candidate| Pair {
                        display: candidate.show,
                        replacement: candidate.insert,
                    })
                    .collect(),
            ),
            None => (pos, Vec::new()),
        })
    }
}

impl Validator for Repl {
    fn validate(&self, ctx: &mut ValidationContext) -> Result<ValidationResult> {
        if is_complete(ctx.input()) {
            Ok(ValidationResult::Valid(None))
        } else {
            Ok(ValidationResult::Incomplete)
        }
    }
}

/// 匹配一个关键字前缀（大小写不敏感），要求后面跟空白或到底。
fn strip_command<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let n = command.len();
    // 防止在多字节字符中间切开（split_at 会 panic）
    if text.len() < n || !text.is_char_boundary(n) {
        return None;
    }
    let (head, rest) = text.split_at(n);
    if !head.eq_ignore_ascii_case(command) {
        return None;
    }
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest)
    } else {
        None
    }
}

fn banner() {
    println!("ydbms — 空库（没有任何内置表）。");
    println!();
    println!("  <SQL>              直接回车执行；多行会攒到分号才执行");
    println!("  Tab                补全（元命令、关键字、表名、列名、路径）");
    println!("  EXPLAIN <SQL>      只打印降级出来的线性 IR");
    println!();
    println!("  \\i <脚本路径>      执行脚本（脚本里除 \\! 外的元命令都能用，可以嵌套）");
    println!("  \\o [文件]          重定向**查询结果表**；不给文件名则回到标准输出");
    println!("  \\d [表名]          列出所有表 / 列出某张表的列");
    println!("  \\dt                列出所有表");
    println!("  \\timing [on|off]   每条语句后打印耗时；不给参数是切换");
    println!("  \\set ON_ERROR_STOP on   遇到第一条错误就停下（默认继续跑）");
    println!("  \\pwd               当前目录");
    println!("  \\ls [目录]         列目录");
    println!("  \\cd [目录]         切换目录（无参回 $HOME）");
    println!("  \\echo <文本>       原样回显");
    println!("  \\! <shell 命令>    只在交互终端可用（脚本里禁用）");
    println!("  \\q / exit          退出（Ctrl-D 也行，Ctrl-C 丢掉这一行）");
    println!();
    println!("路径按当前工作目录解析。脚本遇错会继续跑并逐条报错——没有事务，不回滚。");
    println!("数据只在内存里，退出即丢。");
    println!();
}
