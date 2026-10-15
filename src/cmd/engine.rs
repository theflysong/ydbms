//! 执行引擎：接受**任务**、驱动 SQL 部分、管住两路输出。
//!
//! 任务的来源只有三种，在引擎里没有区别：
//!
//! * 交互式终端敲进来的一段（[`crate::cmd::repl`]）；
//! * `\i <文件>` 读进来的内容（[`Session::run_file`]）；
//! * 元命令本身（引擎自己执行，见 [`crate::cmd::metacmd`]）。
//!
//! # 与 `ir` 的分工
//!
//! **引擎里没有 SQL 知识**：解析、降级、执行都是 [`crate::ir`] 的事。这一层只管
//! "哪一段是 SQL、哪一段是元命令、结果往哪一路写、出错要不要停"。依赖方向是
//! `cmd` → `ir`，反过来不行。
//!
//! # 为什么这一层在 lib 里而不是 `main.rs`
//!
//! 因为脚本里也要能写元命令（至少 `\i`）。脚本的执行器在 lib（[`crate::ir::Script`]），
//! 元命令层要是留在 `main.rs`，脚本就永远用不了它。
//!
//! # 两路输出
//!
//! [`Session`] 有两个输出去向，这是 `\o` 语义的核心：
//!
//! * **`query_out`** —— 只装**查询结果表**（表头 + 数据行）。`\o <文件>` 换的就是它。
//! * **`messages`** —— 状态行、汇总、错误、以及 `\d` / `\echo` / `\ls` 等元命令的
//!   输出。**`\o` 永远不影响它**——所以"`\o` 仅限于查询输出"这句话是结构上成立的，
//!   而不是靠约定。
//!
//! # 输入怎么切
//!
//! 行导向（psql 的模型）：逐行扫，**行首**（trim 后）是 `\` 就当元命令——先把攒着
//! 的 SQL 块跑掉再执行它；否则追加进 SQL 缓冲区。这样多行 SQL 与插在中间的元命令
//! 都对，且顺序严格保持。
//!
//! # 两个开关
//!
//! `\timing` 与 `\set ON_ERROR_STOP on` 都是**会话状态**（不是某一段脚本的），所以
//! `\i` 里设的会一直生效到会话结束。逐条执行靠 [`Script`]：引擎自己驱动循环，于是
//! "每条语句计时"与"第一条错误就停"都是自然的。
//!
//! # 内存模式与服务模式
//!
//! [`Session::new`] 造出来的是**内存**会话（`ydbms` 那个实验终端、以及测试）：
//! 数据只在内存里、退出即丢。`ysql` 走的是 [`Session::open`]——它绑着磁盘上的
//! 一个库（[`Database`]：库目录 + `catalog.json` + 单写者锁），库里的表在打开时
//! 一次载入符号表，**提交点**再把改动写回去。
//!
//! # 提交点：一次 `run_with` = 一次提交
//!
//! [`Session::run_with`] 就是"一次提交"（一个 piece：交互式敲进来的一段、`\i`
//! 读进来的一个文件、管道里攒够的一句）。它末尾调 [`Session::commit`]：
//!
//! * 只写**脏表**（`CREATE TABLE` / `INSERT` / `UPDATE` 碰过的那几张），不是整库重写；
//! * 顺手删掉"磁盘上有、符号表里已经没了"的表文件（`DROP SCHEMA … CASCADE` 的落点）；
//! * 内存模式下一行 IO 都不做。
//!
//! 于是 `ysql -f filmdb.sql` 那种一整段装载只在末尾落一次盘，而交互式敲一条
//! `INSERT` 一返回就已经在磁盘上了。
//!
//! **没有真事务**（阶段 4）：一条 `UPDATE` 中途出错会留下前面已经改过的行，
//! 一个 piece 里前半段跑的语句也**不会**因为后半段报错而回滚；多表提交是逐表
//! `rename`，崩在中间会留下"改了一半"的库（阶段 4.2 的 `commit.json` 要解决的
//! 就是它，本版不做）。这些都在 [`Session::commit`] 与各语句的文档里写着。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::db::phys::PhysTab;
use crate::db::table::{DbError, Result, TabShape, Table};
use crate::ir::stmt::Databases;
use crate::ir::{Outcome, Script, StmtResult, Symbol, SymbolTable};
use crate::usr::service::{Database, ServiceDir};
use crate::usr::User;

use super::complete::Catalog;
use super::metacmd::{self, MetaCmd};

/// `\i` 的递归深度上限。防止脚本自我包含导致无限递归。
pub const MAX_INCLUDE_DEPTH: usize = 16;

/// 一次提交里的执行流。
///
/// `\set ON_ERROR_STOP on` 之后，遇到第一条错误就变成 [`Flow::Stop`]——它会**穿过
/// `\i` 递归**一路并回最外层，于是脚本从出错处整体停下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// 继续跑后面的。
    Continue,
    /// 停下：当前这段提交、以及所有外层的 `\i` 都不要再往下跑了。
    Stop,
}

/// 输出去向。
pub enum Sink {
    /// 直接写标准输出。
    Stdout,
    /// 写文件（`\o <path>`）。
    File {
        /// 目标路径（只用于提示）。
        path: PathBuf,
        /// 缓冲写句柄。
        handle: BufWriter<File>,
    },
    /// 攒在内存里，供测试断言。
    Buffer(Vec<u8>),
}

impl Sink {
    /// 标准输出。
    pub fn stdout() -> Self {
        Sink::Stdout
    }

    /// 内存缓冲（测试用）。
    pub fn buffer() -> Self {
        Sink::Buffer(Vec::new())
    }

    /// 打开一个文件作为去向。
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Sink::File {
            path: path.to_path_buf(),
            handle: BufWriter::new(File::create(path)?),
        })
    }

    /// 人的描述（`\o` 改变去向时提示用）。
    pub fn describe(&self) -> String {
        match self {
            Sink::Stdout => "标准输出".to_string(),
            Sink::File { path, .. } => path.display().to_string(),
            Sink::Buffer(_) => "内存缓冲".to_string(),
        }
    }

    /// `Buffer` 模式下的内容；其它模式返回空串。
    pub fn text(&self) -> String {
        match self {
            Sink::Buffer(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => String::new(),
        }
    }

    fn write(&mut self, text: &str) {
        match self {
            Sink::Stdout => {
                let _ = io::stdout().write_all(text.as_bytes());
            }
            Sink::File { handle, .. } => {
                let _ = handle.write_all(text.as_bytes());
            }
            Sink::Buffer(bytes) => bytes.extend_from_slice(text.as_bytes()),
        }
    }

    fn flush(&mut self) {
        match self {
            Sink::Stdout => {
                let _ = io::stdout().flush();
            }
            Sink::File { handle, .. } => {
                let _ = handle.flush();
            }
            Sink::Buffer(_) => {}
        }
    }
}

/// 输入被切出来的一块。
enum Piece {
    /// 一段 SQL（可能含多条语句）。
    Sql(String),
    /// 一条元命令。
    Meta(MetaCmd),
}

/// 会话**初始**的查询输出是哪种去向。`\o` 无参就是回到它。
///
/// 单独记一个标记而不是硬编码 stdout，是为了让测试能把 Buffer 当作"家"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkHome {
    Stdout,
    Buffer,
}

/// 服务模式下会话绑着的那个库。
///
/// 内存模式是 `None`（见 [`Session::new`]）。字段都在这儿，是因为它们只在提交点
/// 一起用：`dirty` 说"要写哪几张"，`known_on_disk` 说"磁盘上应该有哪些"，
/// 两者的差集就是要删的表文件。
struct Store {
    /// 服务目录（`CREATE DATABASE` / `DROP DATABASE` 的落点）。
    service: ServiceDir,
    /// 打开的库：库目录 + 元数据 + 单写者锁。
    db: Database,
    /// 认证过的用户（owner 判定用它）。
    user: User,
    /// 这次提交要写的表（生效名）。
    dirty: BTreeSet<String>,
    /// 磁盘上**应该有**的表名：载入时那份，加上已经写成功的。
    known_on_disk: BTreeSet<String>,
}

impl Databases for Store {
    fn create_database(&mut self, name: &str) -> Result<String> {
        self.service.create_database(name, &self.user.name)
    }

    fn drop_database(&mut self, name: &str, if_exists: bool) -> Result<String> {
        // 正在用的库不能删：会话的锁自己就握着它，删掉之后后面的提交会往一个
        // 不存在的目录里写（而且别人还连着）。PostgreSQL 也是这条规矩。
        if name == self.db.name() {
            return Err(DbError::Denied {
                message: format!("不能删掉正在使用的库 `{}`（换个库再删它）", name),
            });
        }

        // 别人的库要么是他的、要么自己是管理员——本版没有 GRANT，这就是全部口径。
        // **元数据读不出来就报错**，别当成"没有 owner 限制"：`catalog.json` 坏了
        // 恰恰是最不该让人随手删库的时候（这条在实现里是"宁可报错"的那一侧）。
        if !self.service.exists(name) {
            return self.service.drop_database(name, if_exists);
        }
        let owner = self.service.catalog(name)?.owner;
        if owner != self.user.name && !self.user.is_admin {
            return Err(DbError::Denied {
                message: format!(
                    "库 `{}` 的 owner 是 `{}`，`{}` 不是管理员，不能删",
                    name, owner, self.user.name
                ),
            });
        }

        self.service.drop_database(name, if_exists)
    }
}

/// 一个数据库会话。
pub struct Session {
    symbols: SymbolTable,
    query_out: Sink,
    messages: Sink,
    /// 查询输出最初是哪种去向（`\o` 无参回到它）。
    home: SinkHome,
    depth: usize,
    quit: bool,
    /// `\timing on`：每条语句后打印耗时。
    timing: bool,
    /// `\set ON_ERROR_STOP on`：遇到第一条错误就停。
    on_error_stop: bool,
    /// 上一次提交里 `ON_ERROR_STOP` 有没有让步（见 [`Session::stopped`]）。
    stopped: bool,
    /// 这个会话报过多少次错。`ysql -f` 的退出码用它（见 [`Session::failed`]）。
    failed: usize,
    /// 服务模式：绑着磁盘上的一个库；内存模式是 `None`。
    store: Option<Store>,
}

impl Default for Session {
    fn default() -> Self {
        Session::new()
    }
}

impl Session {
    /// 新建**内存**会话：两路输出都到标准输出，两个开关都关着，数据退出即丢。
    pub fn new() -> Self {
        Session {
            symbols: SymbolTable::new(),
            query_out: Sink::stdout(),
            messages: Sink::stdout(),
            home: SinkHome::Stdout,
            depth: 0,
            quit: false,
            timing: false,
            on_error_stop: false,
            stopped: false,
            failed: 0,
            store: None,
        }
    }

    /// 指定两路输出去向（测试用）。查询输出给 `Buffer` 时，`\o` 无参会回到它。
    pub fn with_sinks(query_out: Sink, messages: Sink) -> Self {
        let mut session = Session::new();
        session.home = match query_out {
            Sink::Buffer(_) => SinkHome::Buffer,
            _ => SinkHome::Stdout,
        };
        session.query_out = query_out;
        session.messages = messages;
        session
    }

    /// 打开磁盘上的一个库：库里的表一次载入符号表，两路输出到标准输出。
    ///
    /// `db` 已经上过单写者锁（见 [`ServiceDir::open`]）——**先上锁再载入**，
    /// 否则会在载入的半途被另一个写者改文件。
    pub fn open(db: Database, user: User, service: ServiceDir) -> Result<Self> {
        Session::open_with_sinks(db, user, service, Sink::stdout(), Sink::stdout())
    }

    /// 同 [`Session::open`]，但指定两路输出去向（测试用）。
    pub fn open_with_sinks(
        db: Database,
        user: User,
        service: ServiceDir,
        query_out: Sink,
        messages: Sink,
    ) -> Result<Self> {
        let mut session = Session::with_sinks(query_out, messages);
        let tables = db.load_tables()?;

        // 整批原子：任何一张读不回来就整体失败，不留下装了一半的符号表
        let mut known_on_disk = BTreeSet::new();
        let mut batch = Vec::with_capacity(tables.len());
        for table in tables {
            known_on_disk.insert(table.name().to_string());
            batch.push(Symbol::relation(table.name().to_string(), table));
        }
        session.symbols.extend(batch)?;

        session.store = Some(Store {
            service,
            db,
            user,
            dirty: BTreeSet::new(),
            known_on_disk,
        });
        Ok(session)
    }

    /// 符号表（只读）。
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// 符号表（可变）。
    pub fn symbols_mut(&mut self) -> &mut SymbolTable {
        &mut self.symbols
    }

    /// `Buffer` 模式下，只装查询结果表的那一路。
    pub fn query_output(&self) -> String {
        self.query_out.text()
    }

    /// `Buffer` 模式下，状态行 / 错误 / 元命令输出的那一路。
    pub fn messages(&self) -> String {
        self.messages.text()
    }

    /// 收到 `\q` 了吗。
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// `\timing` 开着吗。
    pub fn timing(&self) -> bool {
        self.timing
    }

    /// `\set ON_ERROR_STOP on` 开着吗。
    pub fn on_error_stop(&self) -> bool {
        self.on_error_stop
    }

    /// 上一次提交有没有因为 `ON_ERROR_STOP` 停下来。
    ///
    /// 交互终端用它做不了什么（那一段本来就已经停了），**脚本路径**要用它：
    /// 从管道喂进来的一串语句是一条一条提交的，得靠它决定"后面的还读不读"
    /// ——psql 在 `ON_ERROR_STOP` 命中时也是整个脚本停下。
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// 这个会话报过多少次错（含元命令与 `\i` 读文件失败）。
    ///
    /// 报错一律走 [`Session::error`]，所以它就是"这次会话干净不干净"的计数：
    /// `ysql -f` 拿它决定退出码，交互式不看它。
    pub fn failed(&self) -> usize {
        self.failed
    }

    /// 服务模式下打开的库名；内存模式是 `None`。
    pub fn database(&self) -> Option<&str> {
        self.store.as_ref().map(|store| store.db.name())
    }

    /// 服务目录的根；内存模式是 `None`。
    pub fn service_root(&self) -> Option<&Path> {
        self.store.as_ref().map(|store| store.service.root())
    }

    /// 这个会话的用户名；内存模式是 `None`。
    pub fn user(&self) -> Option<&str> {
        self.store.as_ref().map(|store| store.user.name.as_str())
    }

    /// Tab 补全用的目录：现在有哪些表、哪些列。
    ///
    /// **每次补全前现算**——上一条语句刚建出来的表，下一条就该能补出来。
    pub fn catalog(&self) -> Catalog {
        Catalog::from_symbols(&self.symbols)
    }

    /// 交互输入：**全部**元命令可用（含 `\!`）。
    pub fn run_interactive(&mut self, input: &str) {
        let _ = self.run_with(input, true);
    }

    /// 脚本内容：除 `\!` 外全部可用。
    pub fn run_script(&mut self, input: &str) {
        let _ = self.run_with(input, false);
    }

    /// `\i`：读一个脚本文件，把内容交给引擎。
    ///
    /// 这是"任务"的第二种来源——在引擎里与交互式终端敲进来的一段没有区别。
    pub fn run_file(&mut self, path: &Path) {
        let _ = self.include_file(path);
    }

    /// [`run_file`](Session::run_file) 的实现：返回 [`Flow`]，好让"出错就停"**穿过递归**
    /// 传出去（`\i` 里再 `\i`，最里面出错要能把外面也停下）。
    fn include_file(&mut self, path: &Path) -> Flow {
        if self.depth >= MAX_INCLUDE_DEPTH {
            return self.error(&format!(
                "错误：\\i 嵌套超过 {} 层，拒绝继续（脚本可能自我包含）\n",
                MAX_INCLUDE_DEPTH
            ));
        }
        match std::fs::read_to_string(path) {
            Ok(sql) => {
                self.msg(&format!("读入 {}（{} 字节）\n", path.display(), sql.len()));
                self.depth += 1;
                let flow = self.run_with(&sql, false);
                self.depth -= 1;
                flow
            }
            Err(e) => self.error(&format!("错误：读不了 {}：{}\n", path.display(), e)),
        }
    }

    /// `EXPLAIN <SQL>`：只降级不执行，把计划打到消息输出。
    ///
    /// 这不是元命令（不以 `\` 开头），是交互终端的一个显示便利，所以脚本里用不了。
    pub fn explain(&mut self, sql: &str) {
        match crate::ir::compile(sql, &self.symbols) {
            Ok(plan) => match plan.explain(&self.symbols) {
                Ok(rendered) => self.msg(&rendered),
                Err(e) => self.msg(&format!("错误：{}\n", e)),
            },
            Err(e) => self.msg(&format!("错误：{}\n", e)),
        }
    }

    fn run_with(&mut self, input: &str, allow_shell: bool) -> Flow {
        // 每次提交都是新的一轮：上次停下不代表这次也停
        self.stopped = false;

        for piece in split(input) {
            if self.quit {
                break;
            }
            let flow = match piece {
                Piece::Sql(sql) => self.run_sql(&sql),
                Piece::Meta(cmd) => self.exec_meta(cmd, allow_shell),
            };
            if flow == Flow::Stop {
                self.flush();
                // 停下来也要提交：前面跑成功的语句**没有**回滚（本版没有事务），
                // 不落盘就等于"报了错、还把已经改好的数据丢了"
                return match self.commit() {
                    Ok(()) => Flow::Stop,
                    Err(e) => self.error(&format!("错误：提交失败：{}\n", e)),
                };
            }
        }

        self.flush();
        match self.commit() {
            Ok(()) => Flow::Continue,
            Err(e) => self.error(&format!("错误：提交失败：{}\n", e)),
        }
    }

    /// 提交点：把**脏表**写回磁盘，并把已经不在符号表里的表文件删掉。
    ///
    /// 见文件头的「提交点」一节。内存模式直接返回。写完一张、认一张（失败时
    /// `dirty` 里留着它，下次提交再试），所以**一次提交不是原子的**——多张表
    /// 崩在中间会留下"改了一半"的库。
    pub fn commit(&mut self) -> Result<()> {
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };

        // 磁盘上应该有、符号表里已经没有的表：`DROP SCHEMA … CASCADE` 删掉的那些。
        // 别名不在磁盘上，所以只数关系符号（`relation_exact` 不认 `search_path`、
        // 也不穿别名，判的正是"这个名字下有没有一张真表"）。
        let live: BTreeSet<String> = self
            .symbols
            .names()
            .into_iter()
            .filter(|name| self.symbols.relation_exact(name).is_some())
            .map(|name| name.to_string())
            .collect();
        let gone: Vec<String> = store.known_on_disk.difference(&live).cloned().collect();

        for name in store.dirty.clone() {
            // **精确**取表：脏表记的是它自己的生效名，不能再按当时的 `search_path`
            // 解一次——`s.t` 与 `t` 都在、`search_path` 又指向 `s` 时，那会把表存错
            let table = match self.symbols.relation_exact(&name) {
                Some(table) => table,
                // 这一段里刚建出来又被 `DROP SCHEMA … CASCADE` 清掉的表：
                // 没有东西要写，也不算错（它的文件压根没落过盘）
                None => {
                    store.dirty.remove(&name);
                    continue;
                }
            };
            store.db.save_table(table)?;
            store.dirty.remove(&name);
            store.known_on_disk.insert(name);
        }

        for name in gone {
            store.db.remove_table_files(&name)?;
            store.known_on_disk.remove(&name);
        }

        Ok(())
    }

    /// 记下这次提交要写哪张表。
    ///
    /// 只有三条语句会改表内容或表的存在性：`CREATE TABLE` / `INSERT` / `UPDATE`
    /// ——它们的 [`Outcome`] 正好带着生效名，于是脏标记不需要额外的钩子。
    /// `DROP SCHEMA … CASCADE` 不用记：那些表从符号表里消失，[`Session::commit`]
    /// 靠"磁盘上有、符号表里没有"把它们找出来。
    fn mark_dirty(&mut self, result: &StmtResult) {
        let Some(store) = self.store.as_mut() else {
            return;
        };

        let name = match &result.outcome {
            Ok(Outcome::Created { name, .. }) => name,
            Ok(Outcome::Inserted { table, .. }) => table,
            Ok(Outcome::Updated { table, .. }) => table,
            _ => return,
        };
        store.dirty.insert(name.clone());
    }

    fn flush(&mut self) {
        self.query_out.flush();
        self.messages.flush();
    }

    fn msg(&mut self, text: &str) {
        self.messages.write(text);
    }

    fn query(&mut self, text: &str) {
        self.query_out.write(text);
    }

    /// 报一个错。`ON_ERROR_STOP` 开着就**停下**。
    ///
    /// 错误一律走这里、别直接 `msg`——否则"遇错就停"这件事迟早有地方漏掉，
    /// 而且 [`Session::failed`] 的计数也会漏（`ysql -f` 的退出码靠它）。
    fn error(&mut self, text: &str) -> Flow {
        self.failed += 1;
        self.msg(text);
        if self.on_error_stop {
            self.msg("（ON_ERROR_STOP：已停止后续语句）\n");
            self.stop()
        } else {
            Flow::Continue
        }
    }

    /// 停下来：既返回给上层（穿过 `\i` 递归），也留个标记（脚本路径要看）。
    fn stop(&mut self) -> Flow {
        self.stopped = true;
        Flow::Stop
    }

    // ------------------------------------------------------------ SQL

    /// 跑一段 SQL：**逐条**降级 + 执行，于是能计时、能按 `ON_ERROR_STOP` 停下。
    fn run_sql(&mut self, sql: &str) -> Flow {
        let script = match Script::parse(sql) {
            Ok(script) => script,
            Err(e) => {
                // 解析这一步就失败了，一条都没跑
                return self.error(&format!("错误：{}\n", e));
            }
        };

        if script.is_empty() {
            return Flow::Continue;
        }

        let mut failed = 0usize;
        for index in 0..script.len() {
            let began = Instant::now();
            // 服务模式下 `CREATE DATABASE` / `DROP DATABASE` 要落到服务目录，
            // 所以两条执行入口分开——内存模式那条由 `NoDatabases` 明确拒绝。
            let result = match self.store.as_mut() {
                Some(store) => script.run_one_with(index, &mut self.symbols, store),
                None => script.run_one(index, &mut self.symbols),
            };
            let elapsed = began.elapsed();

            self.report(&result);
            self.mark_dirty(&result);
            if self.timing {
                // 自己乘而不是 `as_millis`：毫秒下面还要三位小数
                self.msg(&format!("耗时 {:.3} ms\n", elapsed.as_secs_f64() * 1e3));
            }

            if result.outcome.is_err() {
                failed += 1;
                // 语句自己的失败不走 `Session::error`（那是引擎级错误的漏斗），
                // 所以这里也要给 `failed` 计数——`ysql -f` 的退出码靠它
                self.failed += 1;
                if self.on_error_stop {
                    // 停在 `index` 条上，前面 index 条（0 起）都成功了
                    self.msg(&format!("成功 {} 条，失败 {} 条\n", index, failed));
                    self.msg("（ON_ERROR_STOP：已停止后续语句）\n");
                    return self.stop();
                }
            }
        }

        self.msg(&format!(
            "成功 {} 条，失败 {} 条\n",
            script.len() - failed,
            failed
        ));
        Flow::Continue
    }

    fn report(&mut self, result: &StmtResult) {
        let head = format!("[{}] {}", result.index, result.text);

        match &result.outcome {
            Ok(Outcome::Rows(table)) => {
                self.msg(&format!("{}  → {} 行\n", head, table.nrows()));
                // 只有结果表本身进 query_out
                let rendered = render_table(table);
                self.query(&rendered);
            }
            Ok(Outcome::Created {
                name,
                unenforced,
                checks,
            }) => {
                // 强制了什么、没强制什么都要如实说：`CHECK` 是真的在写行时求值的，
                // 而 `unenforced` 里的那些**只是记下来**
                let enforced = match checks {
                    0 => String::new(),
                    n => format!("（{} 条 CHECK 已强制）", n),
                };
                if unenforced.is_empty() {
                    self.msg(&format!("{}  → 建表成功：{}{}\n", head, name, enforced));
                } else {
                    self.msg(&format!(
                        "{}  → 建表成功：{}{}（{}：已接受但**未强制**）\n",
                        head,
                        name,
                        enforced,
                        unenforced.join("、")
                    ));
                }
            }
            Ok(Outcome::Inserted { table, rows }) => {
                self.msg(&format!("{}  → 插入 {} 行到 {}\n", head, rows, table));
            }
            Ok(Outcome::Updated { table, rows }) => {
                self.msg(&format!("{}  → 更新 {} 行到 {}\n", head, rows, table));
            }
            Ok(Outcome::NoOp { message }) => {
                self.msg(&format!("{}  → {}\n", head, message));
            }
            Err(e) => self.msg(&format!("{}  → 错误：{}\n", head, e)),
        }
    }

    // ------------------------------------------------------ 元命令

    fn exec_meta(&mut self, cmd: MetaCmd, allow_shell: bool) -> Flow {
        match cmd {
            MetaCmd::Quit => {
                self.quit = true;
                Flow::Continue
            }

            MetaCmd::Unknown(name) => self.error(&format!(
                "未知元命令 {}。可用：{}\n",
                name,
                metacmd::NAMES.join(" ")
            )),

            MetaCmd::Echo(text) => {
                self.msg(&format!("{}\n", text));
                Flow::Continue
            }

            MetaCmd::Pwd => match std::env::current_dir() {
                Ok(dir) => {
                    self.msg(&format!("{}\n", dir.display()));
                    Flow::Continue
                }
                Err(e) => self.error(&format!("错误：取不到当前目录：{}\n", e)),
            },

            MetaCmd::Cd(target) => self.change_dir(target),

            MetaCmd::Ls(target) => self.list_dir(target),

            MetaCmd::Include(None) => {
                self.error("用法：\\i <脚本路径>，例如 \\i tests/script/db1/db.sql\n")
            }
            MetaCmd::Include(Some(path)) => self.include_file(&path),

            MetaCmd::Shell(command) => self.shell(command, allow_shell),

            MetaCmd::ListTables => self.list_tables(),

            MetaCmd::Describe(None) => self.list_tables(),
            MetaCmd::Describe(Some(name)) => self.describe_table(&name),

            MetaCmd::Output(None) => {
                self.query_out = match self.home {
                    SinkHome::Stdout => Sink::stdout(),
                    SinkHome::Buffer => Sink::buffer(),
                };
                let described = self.query_out.describe();
                self.msg(&format!("查询输出：{}\n", described));
                Flow::Continue
            }
            MetaCmd::Output(Some(path)) => match Sink::open(&path) {
                Ok(sink) => {
                    self.query_out = sink;
                    self.msg(&format!("查询输出：{}\n", path.display()));
                    Flow::Continue
                }
                // 打不开就**保持原来的去向**——不能把输出弄丢
                Err(e) => {
                    let kept = self.query_out.describe();
                    self.error(&format!(
                        "错误：打不开 {}：{}（查询输出保持为 {}）\n",
                        path.display(),
                        e,
                        kept
                    ))
                }
            },

            MetaCmd::Timing(value) => self.set_timing(&value),

            MetaCmd::Set { name, value } => self.set_variable(&name, value.as_deref()),
        }
    }

    /// `\timing [on|off|1|0|true|false]`；不给参数是**切换**。
    fn set_timing(&mut self, value: &str) -> Flow {
        if value.is_empty() {
            self.timing = !self.timing;
        } else {
            match parse_bool(value) {
                Some(on) => self.timing = on,
                None => {
                    return self.error(&format!(
                        "错误：\\timing 只认 on / off（收到 `{}`）\n",
                        value
                    ))
                }
            }
        }
        self.msg(&format!("计时：{}\n", if self.timing { "开" } else { "关" }));
        Flow::Continue
    }

    /// `\set <变量> [值]`。本版只认 `ON_ERROR_STOP`。
    fn set_variable(&mut self, name: &str, value: Option<&str>) -> Flow {
        if !name.eq_ignore_ascii_case("ON_ERROR_STOP") {
            return self.error(&format!(
                "错误：\\set 只认 ON_ERROR_STOP（收到 `{}`）\n",
                name
            ));
        }

        let on = match value {
            Some(text) => match parse_bool(text) {
                Some(on) => on,
                None => {
                    return self.error(&format!(
                        "错误：\\set ON_ERROR_STOP 只认 on / off（收到 `{}`）\n",
                        text
                    ))
                }
            },
            // 不带值时与 psql 一样当成"打开"
            None => true,
        };

        self.on_error_stop = on;
        self.msg(&format!(
            "ON_ERROR_STOP：{}\n",
            if on { "开" } else { "关" }
        ));
        Flow::Continue
    }

    fn change_dir(&mut self, target: Option<PathBuf>) -> Flow {
        let dir = match target {
            Some(dir) => dir,
            None => match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home),
                None => return self.error("错误：没有 $HOME，请给出目录\n"),
            },
        };

        match std::env::set_current_dir(&dir) {
            Ok(()) => {
                let now = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| dir.display().to_string());
                self.msg(&format!("目录：{}\n", now));
                Flow::Continue
            }
            Err(e) => self.error(&format!("错误：切不到 {}：{}\n", dir.display(), e)),
        }
    }

    fn list_dir(&mut self, target: Option<PathBuf>) -> Flow {
        let dir = target.unwrap_or_else(|| PathBuf::from("."));

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => return self.error(&format!("错误：读不了 {}：{}\n", dir.display(), e)),
        };

        // 先收成 owned，免得一边借 self.symbols 一边 &mut self 去写
        let mut names: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            names.push(if is_dir { format!("{}/", name) } else { name });
        }
        names.sort();

        if names.is_empty() {
            self.msg("（空目录）\n");
        }
        for name in names {
            self.msg(&format!("{}\n", name));
        }
        Flow::Continue
    }

    fn shell(&mut self, command: String, allow_shell: bool) -> Flow {
        if !allow_shell {
            return self.error(
                "错误：脚本里不允许 \\!（执行 shell 命令）。请在交互终端里直接敲。\n",
            );
        }
        if command.is_empty() {
            return self.error("用法：\\! <shell 命令>\n");
        }

        match std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
        {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

                if !stdout.is_empty() {
                    self.msg(&stdout);
                    if !stdout.ends_with('\n') {
                        self.msg("\n");
                    }
                }
                if !stderr.is_empty() {
                    self.msg(&format!("[stderr] {}", stderr));
                    if !stderr.ends_with('\n') {
                        self.msg("\n");
                    }
                }
                if !out.status.success() {
                    match out.status.code() {
                        Some(code) => self.msg(&format!("退出码：{}\n", code)),
                        None => self.msg("进程被信号终止\n"),
                    }
                }
                Flow::Continue
            }
            Err(e) => self.error(&format!("错误：起不了 sh：{}\n", e)),
        }
    }

    fn list_tables(&mut self) -> Flow {
        let names: Vec<String> = self
            .symbols
            .names()
            .iter()
            .map(|n| n.to_string())
            .collect();

        if names.is_empty() {
            self.msg("（没有任何表）\n");
            return Flow::Continue;
        }

        let rows: Vec<(String, usize, usize)> = names
            .into_iter()
            .filter_map(|name| {
                let table = self.symbols.relation(&name)?;
                let arity = self.symbols.columns(&name)?.len();
                Some((name, table.nrows(), arity))
            })
            .collect();

        self.msg(&format!("{:<28}{:>8}{:>8}\n", "表", "行数", "列数"));
        for (name, rows_count, arity) in rows {
            self.msg(&format!("{:<28}{:>8}{:>8}\n", name, rows_count, arity));
        }
        Flow::Continue
    }

    fn describe_table(&mut self, name: &str) -> Flow {
        let columns = match self.symbols.columns(name) {
            Some(columns) => columns
                .iter()
                .map(|c| (c.name.clone(), c.ty.to_string()))
                .collect::<Vec<_>>(),
            None => return self.error(&format!("错误：没有表 `{}`\n", name)),
        };

        self.msg(&format!("表 {}\n", name));
        for (column, ty) in columns {
            self.msg(&format!("  {:<24}{}\n", column, ty));
        }

        // 索引是**目录对象**（TODO 3.6）：`CREATE INDEX` 建的那几条也一起报出来，
        // 否则用户没有任何办法知道"这条查询为什么走了索引"
        let indexes = self.symbols.indexes_of(name);
        if !indexes.is_empty() {
            self.msg("索引\n");
            for (index, at) in indexes {
                let column = self
                    .symbols
                    .columns(name)
                    .and_then(|columns| columns.get(at))
                    .map(|column| column.name.clone())
                    .unwrap_or_else(|| at.to_string());
                self.msg(&format!("  {:<24}({})\n", index, column));
            }
        }
        Flow::Continue
    }
}

/// `on` / `off` / `1` / `0` / `true` / `false`（不分大小写）；别的写法返回 `None`。
///
/// 认不出来就得报错，**不能默认成 off**——把 `\set ON_ERROR_STOP ture` 悄悄当成关着，
/// 比报错危险得多。
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" => Some(true),
        "off" | "0" | "false" => Some(false),
        _ => None,
    }
}

/// 把输入切成 SQL 块与元命令，保持原顺序。
fn split(input: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut sql = String::new();

    for line in input.lines() {
        if line.trim_start().starts_with('\\') {
            // 元命令只在行首算：先把攒着的 SQL 跑掉，保证顺序
            if !sql.trim().is_empty() {
                pieces.push(Piece::Sql(std::mem::take(&mut sql)));
            } else {
                sql.clear();
            }
            if let Some(cmd) = metacmd::parse(line) {
                pieces.push(Piece::Meta(cmd));
            }
        } else {
            sql.push_str(line);
            sql.push('\n');
        }
    }

    if !sql.trim().is_empty() {
        pieces.push(Piece::Sql(sql));
    }

    pieces
}

/// 把结果表渲染成文本块。
fn render_table(table: &PhysTab) -> String {
    let header = table
        .columns()
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(" | ");

    let mut out = String::new();
    out.push_str(&header);
    out.push('\n');
    out.push_str(&"-".repeat(header.len().max(1)));
    out.push('\n');

    for r in 0..table.nrows() {
        if let Some(row) = table.row(r) {
            let cells: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
    }

    out
}
