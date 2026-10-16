//! `ysql` 的命令行:参数 → 认证 → 打开库 → 跑脚本或进终端.
//!
//! # 参数
//!
//! ```text
//! ysql [-U 用户] [-d 库] [-f 脚本] [--server-dist 目录] [--storage heap|csv] [-h]
//! ```
//!
//! `-U` / `-d` / `-f` 照 psql 的习惯;`--server-dist` 与 `--storage` 是本项目自己的
//! 说法.`--storage` 定的是**新建表**用哪种格式(见 [`Storage`]),不给就用这个可执行
//! 文件的预设:[`run`] 给堆,`ysql-csv` 那一支给 CSV([`run_with`]).手写解析:现有
//! 依赖里没有 `clap`,而这里只认五个选项--为它引一个参数库不划算.
//!
//! # 口令从哪来
//!
//! `PGPASSWORD`.没有它时:标准输入是终端就**提示输入**(回显,本版不做
//! 关回显所需的那套终端设置),不是终端就**明确报错**--"非交互还等着人来敲"
//! 是脚本里最难查的一种挂住.
//!
//! # 首次启动的 bootstrap
//!
//! 服务目录里没有 `users.json` 时,把这次 `-U` 指定的用户建成**超级用户**
//! (`LOGIN` + `CREATEDB` + `CREATEROLE` + `INHERIT`,照 initdb 的口径,否则谁都
//! 进不去),并按"没有 `-d` 时默认库 = 用户同名"的规矩把默认库也建出来.
//! **显式给了 `-d` 而库不存在**则报错(建库只有 `CREATE DATABASE` 一条路,
//! 而它要先连上一个库)--这条"没有 `-d` 就自动建"的例外是两个都需要的折中,
//! 写在这里,也在 `--help` 里.
//!
//! # 退出码
//!
//! * `0` -- 正常;
//! * `1` -- 用法 / 认证 / 打开库失败,或者 `-f` 的脚本里**报过**错
//!   ([`Session::failed`] 计数,交互式不看它).
//!
//! **警告不计入**:`BEGIN` 套 `BEGIN`,无事务的 `COMMIT` / `ROLLBACK` 照 PostgreSQL 的
//! 口径只发一句警告(这是有意的选择,见 [`super::engine`] 的文件头).
//!
//! # 事务还开着就退出 = 丢掉它(与 PostgreSQL 断开连接一致)
//!
//! 没有显式 `BEGIN` 时每条语句自己提交一次,所以"写完就落盘";但 `-f` 的脚本**忘了写
//! `COMMIT`** 时那些改动只在内存里,进程一退就没了--与 psql 在断开时回滚开着的事务是
//! 同一条规矩.本版**不为它加一句提示**:psql 也不提示,而多出来的输出会打乱"`-f` 与
//! `\i` 输出逐字一致"这条约定.
//!
//! # 长脚本请包在 `BEGIN; ... COMMIT;` 里
//!
//! 一条语句 = 一个事务,所以没包事务的脚本里**每条写语句都要提交一次**:堆表(默认)
//! 每次提交只把改动的那几行追加下去,而 **CSV 表**(`ysql-csv` / `--storage csv`)是
//! 整表重写一遍.`filmdb.sql` 那种 7.6 万条 INSERT 包了事务,只在末尾写一次;不包就是
//! 另一回事了.这一句在 `-h` 的帮助里也写着.
//!
//! [`Session::failed`]: crate::cmd::Session::failed

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use super::engine::Session;
use super::repl;
use crate::pers::heap::DEFAULT_POOL_BYTES;
use crate::pers::Storage;
use crate::usr::service::ServiceDir;
use crate::usr::{RoleAttrs, User, UserStore};

/// `-h` / `--help` 打出来的帮助;参数报错时也把它跟在后面.
///
/// 是**函数**不是 `const`:文案走 [`tr!`](crate::tr),得在运行时按语言查表.
pub fn help() -> String {
    tr!("cmd.cli.001").into_owned()
}

/// `--storage` 的值:`heap` 或 `csv`,别的一律报用法错.
///
/// 认全名不认缩写(`h` / `c`),也不认大小写混写:这一个开关决定表怎么落盘,
/// "猜一个"比报错糟糕得多.
fn parse_storage(value: &str) -> std::result::Result<Storage, String> {
    match value {
        "heap" => Ok(Storage::Heap),
        "csv" => Ok(Storage::Csv),
        other => Err(format!("{}", tr!("cmd.cli.028", arg0 =  other, arg1 =  help()))),
    }
}

/// 命令行**字面**给出的参数:`None` 表示没给(默认值在 [`resolve`] 里补).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `-U <用户>`.
    pub user: Option<String>,
    /// `-d <库>`.
    pub database: Option<String>,
    /// `-f <脚本>`.
    pub script: Option<PathBuf>,
    /// `--server-dist <目录>`.
    pub server_dist: Option<PathBuf>,
    /// `--storage <heap|csv>`:新建表用哪种格式.
    ///
    /// **字面**给出的值(没给就是 `None`,由 [`run_with`] 的预设补上).
    pub storage: Option<Storage>,
    /// `-h` / `--help`.
    pub help: bool,
}

/// 补上默认值之后的参数.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// 连接的用户(`-U` 或 `$USER`).
    pub user: String,
    /// 连接的库(`-d` 或与用户同名).
    pub database: String,
    /// 命令行**显式**给了 `-d`(决定"库不存在时是报错还是建").
    pub database_given: bool,
    /// `-f <脚本>`.
    pub script: Option<PathBuf>,
    /// 服务目录.
    pub server_dist: PathBuf,
    /// 新建表用哪种格式(字面给了就用字面的,否则是这个可执行文件的预设).
    pub storage: Storage,
}

/// 解析参数(`argv` **不含**程序名).认不出来的,缺值的都在 `Err` 里带上用法.
///
/// 纯函数:不读环境,不碰文件系统,默认值留给 [`resolve`]--于是两者都能单测.
pub fn parse(argv: &[String]) -> std::result::Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;

    while index < argv.len() {
        let arg = argv[index].as_str();

        if arg == "-h" || arg == "--help" {
            options.help = true;
            index += 1;
            continue;
        }

        // 贴写:`-U用户` / `--server-dist=目录`
        if let Some((flag, value)) = split_attached(arg) {
            set_value(&mut options, flag, value.to_string())?;
            index += 1;
            continue;
        }

        if is_flag(arg) {
            let value = argv
                .get(index + 1)
                .ok_or_else(|| format!("{}", tr!("cmd.cli.002", arg0 =  arg, arg1 =  help())))?;
            set_value(&mut options, arg, value.clone())?;
            index += 2;
            continue;
        }

        return Err(format!("{}", tr!("cmd.cli.003", arg0 =  arg, arg1 =  help())));
    }

    Ok(options)
}

/// 补默认值:`-U` 取 `$USER`,`-d` 取用户同名,`--server-dist` 取 `$HOME/.ydbms`.
///
/// 环境通过 `getenv` 传进来(不直接读),于是"没有 `$USER`","没有 `$HOME`"
/// 这些情况能单测,也保证**取不到就报错,绝不猜一个目录/用户出来**.
pub fn resolve(
    options: &Options,
    getenv: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Args, String> {
    let user = match &options.user {
        Some(user) if user.is_empty() => return Err(tr!("cmd.cli.004").into_owned()),
        Some(user) => user.clone(),
        None => getenv("USER")
            .filter(|user| !user.is_empty())
            .ok_or_else(|| tr!("cmd.cli.005").into_owned())?,
    };

    let (database, database_given) = match &options.database {
        Some(database) if database.is_empty() => return Err(tr!("cmd.cli.006").into_owned()),
        Some(database) => (database.clone(), true),
        None => (user.clone(), false),
    };

    let server_dist = match &options.server_dist {
        Some(dir) if dir.as_os_str().is_empty() => {
            return Err(tr!("cmd.cli.007").into_owned())
        }
        Some(dir) => dir.clone(),
        None => PathBuf::from(
            getenv("HOME")
                .filter(|home| !home.is_empty())
                .ok_or_else(|| {
                    tr!("cmd.cli.008").into_owned()
                        .to_string()
                })?,
        )
        .join(".ydbms"),
    };

    if let Some(script) = &options.script {
        if script.as_os_str().is_empty() {
            return Err(tr!("cmd.cli.009").into_owned());
        }
    }

    Ok(Args {
        user,
        database,
        database_given,
        script: options.script.clone(),
        server_dist,
        // 预设已经在 [`run_with`] 里注进 `options` 了(字面给的值优先),
        // 所以走到这儿一定有个值;真没有就是内存模式那种直连,给默认的
        storage: options.storage.unwrap_or_default(),
    })
}

/// `ysql` 的入口(预设:堆).返回值就是进程的退出码.
pub fn run() -> i32 {
    run_with(Storage::Heap)
}

/// 同 [`run`],但把**默认的存储模式**换成 `preset`(命令行给了 `--storage` 就听命令行的).
///
/// `ysql-csv` 那一支(`src/bin/ysql-csv.rs`)就是拿 [`Storage::Csv`] 调它:同一个程序,
/// 两个名字,一份代码--不必为它加 cargo feature,也不必把任何一种存储引擎排除出
/// 二进制(两种格式本来就都在里面,见 `src/pers/mod.rs`).
pub fn run_with(preset: Storage) -> i32 {
    // 语言先定下来(读 YDBMS_LANG / LC_ALL / LANG;都认不出来就用默认的 zh-CN):
    // 帮助与每一条错误都是 `tr!` 出来的,晚了就成了"半路换语言"
    crate::i18n::init_from_env();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let getenv = |key: &str| std::env::var(key).ok();

    let mut options = match parse(&argv) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    // 预设只在命令行**没给** `--storage` 时生效
    if options.storage.is_none() {
        options.storage = Some(preset);
    }

    if options.help {
        print!("{}", help());
        return 0;
    }

    let args = match resolve(&options, &getenv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    let mut session = match connect(&args, &getenv) {
        Ok(connected) => connected,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    // `-f`:脚本跑完就退.与 `\i` 是同一条执行路径(`Session::run_file`),差别只有一处:
    // **脚本模式默认安静**(`VERBOSE` 关)--像 psql 的 `-f`,脚本的输出应该是脚本自己
    // 说了算(状态行与汇总都归 `\set VERBOSE on` 管).失败靠 Session::failed 计数
    // 变成非零退出.
    match &args.script {
        Some(script) => {
            session.set_verbose(false);
            session.run_file(script);
            i32::from(session.failed() > 0)
        }
        None => match repl::run_session(&mut session) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{}", tr!("cmd.cli.010", e = e));
                1
            }
        },
    }
}

/// 缓冲池预算(字节):`YDBMS_BUFFER_POOL_MB` 说的那个,没写或不认识就是默认值.
///
/// 认不出来**不报错**:它只是个内存预算,而"没设"与"设错了"对用户来说都是"用默认的"
/// (与 `--server-dist` 那种必须说清的东西不同).
fn buffer_pool_bytes(getenv: &dyn Fn(&str) -> Option<String>) -> usize {
    getenv("YDBMS_BUFFER_POOL_MB")
        .and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|mb| *mb > 0)
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(DEFAULT_POOL_BYTES)
}

/// 认证 → 决定连哪个库 → 打开它.`Err` 是给用户看的一段话.
fn connect(
    args: &Args,
    getenv: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Session, String> {
    let service = ServiceDir::resolve(Some(args.server_dist.clone()), getenv("HOME").map(PathBuf::from))
        .map_err(|e| format!("{}", tr!("cmd.cli.011", e = e)))?
        // 缓冲池预算:页式基表(TODO 7.5)最多常驻多少页由它定,于是 `ysql` 的
        // 常驻内存与库的大小无关.`YDBMS_BUFFER_POOL_MB` 覆盖默认值.
        .with_pool_bytes(buffer_pool_bytes(getenv))
        // 存储模式:新建的表按它落盘(已有表照旧按提交记录里记的格式读写)
        .with_storage(args.storage);

    let user = authenticate(&service, args, getenv)?;

    // 库在不在:显式 `-d` 时不存在就报错(建库只有 CREATE DATABASE 一条路,
    // 而它要先连上一个库);没给 `-d` 的默认库顺手建出来.
    if !service.exists(&args.database) {
        if args.database_given {
            let known = service
                .databases()
                .map_err(|e| format!("{}", tr!("cmd.cli.012", e = e)))?;
            let known = match known.is_empty() {
                true => tr!("cmd.cli.013").into_owned(),
                false => known.join(","),
            };
            return Err(format!("{}", tr!("cmd.cli.014", arg0 = 
                args.database, arg1 =  known, arg2 =  args.database
            )));
        }
        service
            .create_database(&args.database, &user.name)
            .map_err(|e| format!("{}", tr!("cmd.cli.015", e = e)))?;
    }

    let db = service
        .open(&args.database, &user)
        .map_err(|e| format!("{}", tr!("cmd.cli.016", e = e)))?;

    Session::open(db, user, service)
        .map_err(|e| format!("{}", tr!("cmd.cli.017", arg0 =  args.database, e = e)))
}

/// 认证:`users.json` 不在就 bootstrap.
fn authenticate(
    service: &ServiceDir,
    args: &Args,
    getenv: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<User, String> {
    let path = service.users_path();

    if !UserStore::exists(&path) {
        // 首次使用:谁都得先有一个管理员,否则这个服务目录永远进不去
        let prompt = format!("{}", tr!("cmd.cli.018", arg0 =  args.user));
        let password = password_for(getenv, &prompt)?;

        let mut store = UserStore::empty();
        let user = store
            .add(&args.user, Some(&password), RoleAttrs::bootstrap())
            .map_err(|e| format!("{}", tr!("cmd.cli.019", e = e)))?
            .clone();
        store
            .save(&path)
            .map_err(|e| format!("{}", tr!("cmd.cli.020", arg0 =  path.display(), e = e)))?;

        eprintln!("{}", tr!("cmd.cli.021", arg0 = 
            path.display(), arg1 = 
            user.name
        ));
        return Ok(user);
    }

    let store = UserStore::load(&path).map_err(|e| format!("{}", tr!("cmd.cli.022", e = e)))?;
    let prompt = format!("{}", tr!("cmd.cli.023", arg0 =  args.user));
    let password = password_for(getenv, &prompt)?;

    store
        .authenticate(&args.user, &password)
        .map(|user| user.clone())
        .map_err(|e| format!("{}", tr!("cmd.cli.024", e = e)))
}

/// 口令从哪来:`PGPASSWORD` → 交互式提示 → 非交互报错.
fn password_for(
    getenv: &dyn Fn(&str) -> Option<String>,
    prompt: &str,
) -> std::result::Result<String, String> {
    if let Some(password) = getenv("PGPASSWORD") {
        return Ok(password);
    }

    if !io::stdin().is_terminal() {
        return Err(
            tr!("cmd.cli.025").into_owned(),
        );
    }

    // 提示走 stderr:stdout 留给查询结果与消息(与 psql 一致)
    eprint!("{prompt}");
    let _ = io::stderr().flush();

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("{}", tr!("cmd.cli.026", e = e)))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// 认出来的选项名.
fn is_flag(arg: &str) -> bool {
    matches!(arg, "-U" | "-d" | "-f" | "--server-dist" | "--storage")
}

/// 贴写的选项:`-U用户` / `--server-dist=目录`.
///
/// 注意 `=` 那一支**只认已知选项**:值里带 `=` 的贴写(`-fsql/a=b.sql`)要落到
/// 下面短选项那支去,不能因为"含 `=`"就当成未知参数拒绝.
fn split_attached(arg: &str) -> Option<(&str, &str)> {
    if let Some((flag, value)) = arg.split_once('=') {
        if is_flag(flag) {
            return Some((flag, value));
        }
    }

    for flag in ["-U", "-d", "-f"] {
        if arg.len() > flag.len() {
            if let Some(value) = arg.strip_prefix(flag) {
                return Some((flag, value));
            }
        }
    }
    None
}

/// 把一个选项的值记进 [`Options`].
fn set_value(
    options: &mut Options,
    flag: &str,
    value: String,
) -> std::result::Result<(), String> {
    match flag {
        "-U" => options.user = Some(value),
        "-d" => options.database = Some(value),
        "-f" => options.script = Some(PathBuf::from(value)),
        "--server-dist" => options.server_dist = Some(PathBuf::from(value)),
        "--storage" => options.storage = Some(parse_storage(&value)?),
        other => return Err(format!("{}", tr!("cmd.cli.027", arg0 =  other, arg1 =  help()))),
    }
    Ok(())
}
