//! `ysql` 的参数解析 testbench(阶段 6.2).
//!
//! [`parse`] 与 [`resolve`] 都是**纯函数**(不读环境,不碰文件系统),环境通过
//! 一个闭包传进去,所以"没有 `$USER`","没有 `$HOME`"这些情况也能在这里测.
//! 真进程的行为在 `tests/ysql_cli.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ydbms::cmd::cli::{help, parse, resolve, Args, Options};

/// 把参数写成人话(`&["-U", "bob"]`).
fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// 假环境.
fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

/// 一个"什么环境变量都没有"的环境.
fn no_env() -> impl Fn(&str) -> Option<String> {
    |_: &str| None
}

// ------------------------------------------------------------------ 帮助

#[test]
fn help_lists_the_four_options_and_their_defaults() {
    for needle in ["-U", "-d", "-f", "--server-dist", "$USER", "$HOME/.ydbms", "PGPASSWORD"] {
        assert!(help().contains(needle), "帮助里少了 `{needle}`:\n{}", help());
    }
    // 默认值也要写出来
    assert!(help().contains("与 -U 同名"), "{}", help());
}

// ------------------------------------------------------------------ 解析

#[test]
fn the_four_options_are_parsed_with_and_without_a_space() {
    // 分开写
    let options = parse(&argv(&["-U", "bob", "-d", "filmdb", "-f", "s.sql", "--server-dist", "/x"]))
        .expect("解析");
    assert_eq!(
        options,
        Options {
            user: Some("bob".to_string()),
            database: Some("filmdb".to_string()),
            script: Some(PathBuf::from("s.sql")),
            server_dist: Some(PathBuf::from("/x")),
            help: false,
        }
    );

    // 贴写:短选项贴值,长选项用 =
    let attached = parse(&argv(&["-Ubob", "-dfilmdb", "-fs.sql", "--server-dist=/x"])).expect("解析");
    assert_eq!(attached, options);
}

#[test]
fn a_help_flag_is_recognised() {
    assert!(parse(&argv(&["-h"])).expect("解析").help);
    assert!(parse(&argv(&["--help"])).expect("解析").help);
    assert!(!parse(&argv(&["-U", "bob"])).expect("解析").help);
}

#[test]
fn an_unknown_option_or_a_missing_value_is_an_error() {
    for bad in [
        vec!["--nope"],
        vec!["-x"],
        vec!["-U"],                     // 缺值
        vec!["--server-dist"],          // 缺值
        vec!["extra"],                  // 不支持裸参数
        vec!["-U", "bob", "--nope"],    // 后面才出错
        vec!["--nope=1"],               // 带 `=` 的未知长选项
    ] {
        let err = parse(&argv(&bad)).expect_err(&format!("{bad:?} 应当报错"));
        assert!(!err.is_empty());
        // 报错要带上用法,不然用户不知道有哪些选项
        assert!(err.contains("用法"), "{bad:?}: {err}");
    }
}

#[test]
fn an_attached_value_may_contain_an_equals_sign() {
    // `-f脚本` 的值里带 `=`:不能因为"含 `=`"就当成长选项去认
    assert_eq!(
        parse(&argv(&["-fsql/a=b.sql"])).expect("解析").script,
        Some(PathBuf::from("sql/a=b.sql"))
    );
    // 长选项照旧把 `=` 之后全当值
    assert_eq!(
        parse(&argv(&["--server-dist=/x=y"])).expect("解析").server_dist,
        Some(PathBuf::from("/x=y"))
    );
}

// ------------------------------------------------------------ 默认值

#[test]
fn defaults_come_from_user_and_home() {
    let options = parse(&argv(&[])).expect("解析");
    let args = resolve(&options, &env(&[("USER", "bob"), ("HOME", "/home/bob")])).expect("补默认值");

    assert_eq!(
        args,
        Args {
            user: "bob".to_string(),
            database: "bob".to_string(), // -d 默认与用户同名(照 psql 的习惯)
            database_given: false,
            script: None,
            server_dist: PathBuf::from("/home/bob/.ydbms"),
        }
    );
}

#[test]
fn explicit_options_win_over_the_environment() {
    let options = parse(&argv(&["-U", "alice", "-d", "filmdb", "--server-dist", "/srv"]))
        .expect("解析");
    let args = resolve(&options, &env(&[("USER", "bob"), ("HOME", "/home/bob")])).expect("补默认值");

    assert_eq!(args.user, "alice");
    assert_eq!(args.database, "filmdb");
    assert!(args.database_given, "显式给了 -d");
    assert_eq!(args.server_dist, PathBuf::from("/srv"));
}

#[test]
fn a_missing_user_or_home_is_an_error_instead_of_a_guess() {
    let options = parse(&argv(&[])).expect("解析");

    let err = resolve(&options, &no_env()).expect_err("没有 $USER");
    assert!(err.contains("$USER"), "{err}");

    let err = resolve(&options, &env(&[("USER", "bob")])).expect_err("没有 $HOME");
    assert!(err.contains("$HOME"), "{err}");

    // 给了 --server-dist 就不需要 $HOME 了
    let options = parse(&argv(&["--server-dist", "/x"])).expect("解析");
    assert_eq!(
        resolve(&options, &env(&[("USER", "bob")])).expect("够了").server_dist,
        PathBuf::from("/x")
    );

    // $USER 是空串也算没有
    let err = resolve(&options, &env(&[("USER", ""), ("HOME", "/home/bob")]))
        .expect_err("空 $USER 不算数");
    assert!(err.contains("$USER"), "{err}");
}

#[test]
fn empty_values_are_refused() {
    for bad in [
        vec!["-U", ""],
        vec!["-d", ""],
        vec!["--server-dist", ""],
        vec!["-f", ""],
    ] {
        let options = parse(&argv(&bad)).expect("解析");
        let err = resolve(&options, &env(&[("USER", "bob"), ("HOME", "/home/bob")]))
            .expect_err(&format!("{bad:?} 应当报错"));
        assert!(err.contains("错误"), "{bad:?}: {err}");
    }
}
