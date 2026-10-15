//! 交互式终端:把敲进来的东西交给引擎.
//!
//! 终端只做两件引擎管不了的事:**行编辑**(rustyline:Tab 补全,历史,光标)与
//! **"这一句够了吗"**([`is_complete`]).剩下的--SQL,元命令,两路输出--都在
//! [`Session`] 里,交互输入与 `\i` 读进来的内容在引擎看来是同一件事.
//!
//! # 两个入口
//!
//! [`run`] 是内存模式的 `ydbms`(空库,退出即丢);[`run_session`] 是服务模式的
//! `ysql`(会话**已经**打开了一个库,见 [`Session::open`]).两者共用 `interact`:
//! 换了会话与提示符,别的(多行,补全,非 tty 回退)一个字没变.
//!
//! # 多行怎么判断
//!
//! rustyline 的 `Validator` 每次回车都问一次"够了吗":不够就继续收行(这就是
//! "累积到分号才执行"),够了才把**整段**交出去.规则见 [`is_complete`].
//!
//! # 非 tty 怎么办
//!
//! 管道/重定向没有终端可编辑,走 `run_plain`:逐行读,同样按 [`is_complete`]
//! 累积,**EOF 时把攒着的交出去**.于是 `printf 'SELECT 1\n' | ydbms` 照旧能用.
//!
//! # `TERM` 的讲究
//!
//! rustyline 在 `TERM` 是 `dumb` / `cons25` / `emacs` 时**主动降级**(不接手行编辑,
//! 也就没有补全).这是它的既定行为,与 readline 在哑终端上的表现一致.

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

/// 内存模式的入口(`ydbms`):一个空库,数据退出即丢.
pub fn run() -> io::Result<()> {
    // 同 `cli::run`:横幅与提示都是 `tr!` 出来的,先把语言定下来
    crate::i18n::init_from_env();

    let mut session = Session::new();
    banner_memory();
    interact(&mut session)
}

/// 服务模式的入口(`ysql`):会话**已经**打开了一个库(见 [`Session::open`]).
///
/// 提示符**每轮从会话取**([`Session::prompt`])--`\c` 换了库(或 `SET ROLE` 换了
/// 身份)之后,下一条提示符就得跟着变.横幅写清用户,库与服务目录,因为这几样
/// 决定了"数据写到哪儿去了".
pub fn run_session(session: &mut Session) -> io::Result<()> {
    banner_session(session);
    interact(session)
}

/// 读输入,交给引擎,直到 `\q`(或 Ctrl-D / EOF).两种入口共用这一套.
fn interact(session: &mut Session) -> io::Result<()> {
    if !io::stdin().is_terminal() {
        return run_plain(session);
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
        // 每次读之前刷新候选:上一条语句刚建出来的表,这一条就该能补出来
        if let Some(helper) = editor.helper_mut() {
            helper.catalog = session.catalog();
        }

        let prompt = session.prompt();
        match editor.readline(&prompt) {
            Ok(line) => {
                let _ = editor.add_history_entry(line.as_str());
                submit(session, &line);
                if session.should_quit() {
                    break;
                }
            }
            // Ctrl-C:把这一行丢掉,重新来
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl-D:退出
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(as_io_error(e)),
        }
    }

    println!("{}", tr!("cmd.repl.001"));
    Ok(())
}

/// rustyline 的错误转成 [`io::Error`]:`ReadlineError` 不能直接 `?` 进 `io::Result`.
fn as_io_error(e: ReadlineError) -> io::Error {
    io::Error::other(e.to_string())
}

/// 没有终端可编辑时的回退:逐行读,按 [`is_complete`] 累积后再交出去.
///
/// 这一路就是"把脚本喂进来"(`ydbms < s.sql`),所以 **`ON_ERROR_STOP` 命中后不再读
/// 后面的语句**--与 psql 一致(实测:psql 从管道喂带 `ON_ERROR_STOP` 的脚本,
/// 后面那些语句一条都不跑).
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

    // EOF:攒着的也交出去(`SELECT 1` 不打分号也要能跑)
    if !pending.trim().is_empty() {
        submit(session, &pending);
    }
    Ok(())
}

/// 一段输入怎么交给引擎:`exit` 当作 `\q`,`EXPLAIN` 走"只降级不执行"那条路.
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

/// REPL 的"这一句够了吗".够一句的四种情况:
///
/// 1. 去掉空白之后是空的(回车什么也不干);
/// 2. **最后一个非空行**以 `\` 开头--元命令一行就是一句(引擎的 `split` 会先把
///    挂着的 SQL 跑掉再跑它,顺序仍然对);
/// 3. 第一行以 `EXPLAIN` 开头--交互终端的老手感:`EXPLAIN SELECT ...` 不用打分号;
/// 4. SQL 里出现了**顶层分号**(不在字符串,注释,`$$` 里),见
///    [`ir::has_top_level_terminator`].
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

/// rustyline 的接线:补全交给 [`complete::suggest`],多行判定交给 [`is_complete`].
struct Repl {
    /// 补全用的目录;每读一行之前刷新一次.
    catalog: Catalog,
}

impl Helper for Repl {}

// 不用提示(`rustyline` 要求实现这两个 trait)
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

/// 匹配一个关键字前缀(大小写不敏感),要求后面跟空白或到底.
fn strip_command<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let n = command.len();
    // 防止在多字节字符中间切开(split_at 会 panic)
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

/// 内存模式的横幅.
fn banner_memory() {
    println!("{}", tr!("cmd.repl.002"));
    println!();
    banner_commands();
    println!("{}", tr!("cmd.repl.003"));
    println!("{}", tr!("cmd.repl.004"));
    println!();
}

/// 服务模式的横幅:写清用户,库,服务目录与提交口径.
fn banner_session(session: &Session) {
    let db = session.database().unwrap_or("?");
    let user = session.user().unwrap_or("?");

    println!("{}", tr!("cmd.repl.005", arg0 =  db, arg1 =  user));
    if let Some(root) = session.service_root() {
        println!("{}", tr!("cmd.repl.006", arg0 =  root.display()));
        println!("{}", tr!("cmd.repl.007"));
        println!("{}", tr!("cmd.repl.008", arg0 =  db));
        println!("{}", tr!("cmd.repl.009", arg0 =  db));
        println!("{}", tr!("cmd.repl.010", arg0 =  db));
        println!("{}", tr!("cmd.repl.031", arg0 =  db));
    }
    println!();
    banner_commands();
    println!("{}", tr!("cmd.repl.011"));
    println!("{}", tr!("cmd.repl.012"));
    println!();
}

/// 两种模式共用的那串元命令说明.
fn banner_commands() {
    println!("{}", tr!("cmd.repl.013"));
    println!("{}", tr!("cmd.repl.014"));
    println!("{}", tr!("cmd.repl.015"));
    println!();
    println!("{}", tr!("cmd.repl.028"));
    println!("{}", tr!("cmd.repl.029"));
    println!("{}", tr!("cmd.repl.030"));
    println!("{}", tr!("cmd.repl.016"));
    println!("{}", tr!("cmd.repl.017"));
    println!("{}", tr!("cmd.repl.018"));
    println!("{}", tr!("cmd.repl.019"));
    println!("{}", tr!("cmd.repl.020"));
    println!("{}", tr!("cmd.repl.021"));
    println!("{}", tr!("cmd.repl.022"));
    println!("{}", tr!("cmd.repl.023"));
    println!("{}", tr!("cmd.repl.024"));
    println!("{}", tr!("cmd.repl.025"));
    println!("{}", tr!("cmd.repl.026"));
    println!("{}", tr!("cmd.repl.027"));
    println!();
}
