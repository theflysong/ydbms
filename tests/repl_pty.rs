//! 真终端上的验收:用 `script` 给 `ydbms` 开一个 **pty**,把按键**分次**送进去.
//!
//! 为什么必须分次:一次性灌进去会被终端当成"粘贴"(还没进 raw mode 就排进了缓冲区),
//! 那时 Tab 只是一个普通字节,补全根本不会发生.真人打字是有间隔的,这里照做.
//!
//! 这套测试**依赖环境**:`script`(util-linux)与 `timeout` 缺一个就跳过--
//! 补全与多行的核心逻辑已经在 `tests/repl.rs` 里用纯函数测过了.
//!
//! `TERM` 必须设成真终端才有的名字:rustyline 在 `dumb` 上会主动降级(没有补全).

#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// 两次按键之间等多久--给 rustyline 处理完上一个字节的时间.
const KEY_DELAY: Duration = Duration::from_millis(220);
/// 起进程之后等它进 raw mode,打出提示符.
const START_DELAY: Duration = Duration::from_millis(700);

/// 跑测试用的二进制.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ydbms")
}

/// `script` 与 `timeout` 都在吗.
fn have_tools() -> bool {
    ["script", "timeout"].iter().all(|tool| {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}"))
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    })
}

/// 在 pty 里跑一遍:`keys` 依次送进去,返回终端上出现过的全部输出.
fn drive(keys: &[&str]) -> String {
    let mut child = Command::new("timeout")
        .arg("25")
        .arg("script")
        .arg("-qec")
        .arg(format!("'{}'", bin()))
        .arg("/dev/null")
        .env("TERM", "xterm")
        // 横幅与提示是中文,断言也就写中文:钉住语言,别跟着开发机的 LANG 跑
        .env("YDBMS_LANG", "zh-CN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("起不了 script");

    let mut input = child.stdin.take().expect("stdin");
    std::thread::sleep(START_DELAY);

    for key in keys {
        // 进程要是提前退了(慢机器,崩了),管道会断--留给断言去报,别 panic 在这里
        if input.write_all(key.as_bytes()).is_err() || input.flush().is_err() {
            break;
        }
        std::thread::sleep(KEY_DELAY);
    }
    drop(input);

    let out = child.wait_with_output().expect("收输出");
    // pty 会把 `\n` 翻成 `\r\n`(ONLCR):归一化,断言才好写
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

#[test]
fn tab_completes_a_keyword() {
    if !have_tools() {
        return;
    }

    // 只打了 `sel`,能跑出结果就说明 Tab 把它补成了 SELECT
    let out = drive(&["sel\t", " 1 AS n;\n", "\\q\n"]);

    assert!(out.contains("SELECT"), "Tab 没把 sel 补成 SELECT:{out}");
    assert!(out.contains("成功 1 条"), "补出来的语句没跑起来:{out}");
}

#[test]
fn tab_completes_a_metacommand_and_lists_candidates() {
    if !have_tools() {
        return;
    }

    // `\ti` + Tab → `\timing`;`\` + 两次 Tab → 列出候选
    let out = drive(&["\\ti\t", "\n", "\\", "\t", "\t", "\\q\n"]);

    assert!(out.contains("计时:开"), "\\ti 没补成 \\timing:{out}");
    assert!(out.contains("\\set"), "双 Tab 应当把候选列出来:{out}");
    assert!(out.contains("\\dt"), "双 Tab 应当把候选列出来:{out}");
}

#[test]
fn multiline_sql_is_accumulated_until_the_semicolon() {
    if !have_tools() {
        return;
    }

    // 三行,只有最后一行有分号:中途不该提交(否则第一行就会报语法错)
    let out = drive(&[
        "CREATE TABLE m (\n",
        "  a INT\n",
        ");\n",
        "\\q\n",
    ]);

    assert!(out.contains("建表成功:m"), "多行没攒成一条:{out}");
    assert!(
        !out.contains("syntax error"),
        "中途提交过(被当成一句残缺的 SQL):{out}"
    );
}

#[test]
fn a_semicolon_inside_a_string_does_not_submit() {
    if !have_tools() {
        return;
    }

    let out = drive(&["SELECT ';' AS s;\n", "\\q\n"]);
    assert!(out.contains("成功 1 条"), "{out}");
    assert!(out.contains("s\n"), "结果里应当有 s 这一列:{out}");
}

#[test]
fn timing_and_on_error_stop_take_effect_in_the_terminal() {
    if !have_tools() {
        return;
    }

    let out = drive(&[
        "\\timing on\n",
        "\\set ON_ERROR_STOP on\n",
        "SELECT 1 FROM nope;\n",
        "SELECT 2 AS n;\n",
        "\\q\n",
    ]);

    assert!(out.contains("计时:开"), "{out}");
    assert!(out.contains("耗时"), "{out}");
    assert!(out.contains("ON_ERROR_STOP:开"), "{out}");
    assert!(out.contains("已停止后续语句"), "{out}");
    assert!(out.contains("成功 0 条,失败 1 条"), "{out}");
}

#[test]
fn ctrl_d_quits() {
    if !have_tools() {
        return;
    }

    let out = drive(&["\u{4}"]);
    assert!(out.contains("再见"), "Ctrl-D 应当退出:{out}");
}
