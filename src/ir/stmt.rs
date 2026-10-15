//! 语句层:谁产出结果集,谁有副作用.
//!
//! [`Plan`] 是**纯查询 IR**--它描述"算出哪些行",`exec::execute` 的签名是
//! `(&Plan, &SymbolTable) -> PhysTab`,只读,无副作用.它内部是**拉取式**的
//! (`exec::execute_source` 造管道,`execute` 抽干 root),但这里只关心"结果表".DDL/DML 不是这种东西:
//! 它们要改符号表(建表)或改表内容(插数据).所以它们放在这一层,而不是往
//! `Op` 里塞一个 `Insert` 变体--那样会让 `execute` 变成 `&mut`,把"改符号表"
//! 这件事污染进整条查询链路和所有算子.
//!
//! 表达式仍然复用线性 IR:`INSERT` 里的值就是 [`Prog`],白拿 `Prog::verify`
//! 与 `Prog::eval`.`CHECK` 也是--它挂在一层 [`CheckedTable`] 装饰器上,
//! 于是写路径不必各自记得校验.
//!
//! # 为什么必须逐条"降级 → 执行"
//!
//! 后面的语句依赖前面建出来的表:`INSERT INTO prime` 的降级需要 `prime` 已经
//! 在符号表里,而它是上一条 `CREATE TABLE` 刚建的.所以 [`run_script`] 不能
//! 先把整个脚本降级完,只能一条一条来.
//!
//! # 为什么还提供 [`Script`]
//!
//! [`run_script`] 一口气跑完,调用方拿不到"每隔一条"的粒度--而 `\timing`
//! 要量每条语句的耗时,`ON_ERROR_STOP` 要在第一条错误处停下,两者都要求
//! **由调用方驱动循环**.所以把"解析"和"逐条执行"拆开:[`Script::parse`] 解析
//! 一次,[`Script::run_one`] 跑一条(降级仍在执行时做,见上).`run_script`
//! 就是这两步的循环,签名与行为一个字没变.
//!
//! [`has_top_level_terminator`] 是给 REPL 用的:判断"这段输入里有没有一句说完",
//! 于是多行 SQL 能攒到分号才提交,而字符串/注释里的分号不会被误切.

use pg_query::protobuf::RawStmt;

use crate::db::phys::PhysTab;
use crate::db::table::{
    is_file_stem_safe, Column, ColumnsShape, DataType, DbError, Result, Row, TypeFamily, Value,
};

use super::checks::{Check, CheckedTable};

use super::expr::Prog;
use super::plan::Plan;
use super::symbol::{SymbolTable, TxKind};
use super::{exec, irgen};

/// 一条语句.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// 查询:产出一张结果表.纯的,无副作用.
    Query(Plan),

    /// `CREATE TABLE`:在符号表里登记一张空表.
    CreateTable {
        /// 表名(可能带 schema,如 `study.people`).
        name: String,
        /// 列定义.
        columns: Vec<Column>,
        /// 语句里写的是 `CREATE TABLE IF NOT EXISTS`.
        if_not_exists: bool,
        /// **接受但没有强制**的约束,例如 `"PRIMARY KEY (prime_id)"`.
        ///
        /// 这个字段存在的意义就是"不许偷偷不强制":调用方要把它显示出来.
        /// `NOT NULL`,`DEFAULT` 不在其中--它们与 `CHECK` 一样是**真的强制**的
        /// (前两个由 `Table::validate` 与 INSERT 的补值管,`CHECK` 见
        /// [`Check`]).
        unenforced: Vec<String>,
        /// **真的强制**的 `CHECK` 约束:建表时把它们包进
        /// [`CheckedTable`],于是所有写路径都过约束.
        checks: Vec<Check>,
    },

    /// `INSERT ... VALUES`:往表里加行.
    Insert {
        /// 表名.
        table: String,
        /// 每一行按**表的列序**给出全部列的值.
        ///
        /// 降级时已经按语句里的列名表重排过了,所以这里的位置是确定的;值表达式
        /// 没有输入行,作用域为空,出现列引用会被 `Prog::verify` 挡下.
        rows: Vec<Vec<Prog>>,
    },

    /// `CREATE INDEX [IF NOT EXISTS] name ON table (column)`.
    ///
    /// 索引是**目录对象**(见 [`crate::db::index`]):这里只把名字,表,列记下来,
    /// 真正建索引与后续的维护/失效都在 [`SymbolTable`] 与执行器里.
    CreateIndex {
        /// 索引名(可能带 schema 前缀).
        name: String,
        /// 表名.
        table: String,
        /// 建在**这张表**的第几列上.
        column: usize,
        /// 语句里写了 `IF NOT EXISTS`.
        if_not_exists: bool,
    },

    /// `DROP INDEX [IF EXISTS] name`.
    DropIndex {
        /// 索引名.
        name: String,
        /// 语句里写了 `IF EXISTS`.
        if_exists: bool,
    },

    /// `UPDATE ... SET ... [WHERE ...]`:改表里已有的行.
    ///
    /// 一条 `UPDATE` 中途出错时,已经改过的行会被**这一句的隐式事务**盖回去
    /// (语句级原子,见 [`execute_stmt_with`]);显式事务里则由那个事务决定.
    Update {
        /// 表名(生效名).
        table: String,
        /// `(列下标, 值程序)`.值程序面对**整张表**的扁平 schema,并且在**旧行**
        /// 上求值(与 PostgreSQL 一致:所有 `SET` 表达式看到的都是改之前那一行).
        sets: Vec<(usize, Prog)>,
        /// `WHERE` 谓词;`None` 表示所有行都改.同样面对整张表的扁平 schema.
        filter: Option<Prog>,
    },

    /// 事务控制:`BEGIN` / `COMMIT` / `ROLLBACK`(见 [`TxOp`]).
    ///
    /// 真的生效:撤销记录在符号表里(见 [`SymbolTable`] 的文件头).没有 `BEGIN`
    /// 时引擎给**每条语句**开一个隐式事务--成功即提交,失败即回滚.
    Transaction(TxOp),

    /// `CREATE SCHEMA name`:在符号表里记下这个名字.
    ///
    /// 本版没有完整的 schema 系统--限定名本来就是不透明标识符拼出来的--记下它
    /// 只为了 [`SymbolTable::drop_schema`] 能判断"这个 schema 存不存在".
    CreateSchema {
        /// schema 名.
        name: String,
        /// 语句里写了 `IF NOT EXISTS`.
        if_not_exists: bool,
    },

    /// `DROP SCHEMA [IF EXISTS] name [CASCADE]`:删掉该前缀下的所有符号.
    DropSchema {
        /// schema 名.
        name: String,
        /// 语句里写了 `IF EXISTS`.
        if_exists: bool,
        /// 语句里写了 `CASCADE`.
        cascade: bool,
    },

    /// `SET search_path TO ...`.
    SetSearchPath {
        /// 新的搜索路径.
        path: Vec<String>,
    },

    /// `CREATE DATABASE name`:在**服务目录**下建一个库.
    ///
    /// 语句层不认识文件系统,也不该认识:这里只把名字交出去,真正建目录,写
    /// `catalog.json`(含 owner)的是 [`Databases`] 的实现.
    CreateDatabase {
        /// 库名.
        name: String,
    },

    /// `DROP DATABASE [IF EXISTS] name`.
    DropDatabase {
        /// 库名.
        name: String,
        /// 语句里写了 `IF EXISTS`.
        if_exists: bool,
    },

    /// `ALTER DATABASE name SET search_path TO ...`:改**库级**的会话默认值.
    ///
    /// 同样只把名字与取值交出去(见 [`Databases::set_database_search_path`]):
    /// 写进哪个文件,什么格式,语句层不知道也不该知道.
    ///
    /// 它**只影响以后的连接**--当前会话的 `search_path` 不动(与 PostgreSQL 一致).
    AlterDatabase {
        /// 库名.
        name: String,
        /// 新的库级默认 `search_path`;**空**表示清掉(`RESET` / `TO DEFAULT`).
        search_path: Vec<String>,
    },
}

/// 事务控制语句的三种.
///
/// 与 PostgreSQL 的差别只有一处,且是**有意**的:没有 `SAVEPOINT`,没有
/// "事务已中止"状态,也没有嵌套事务--`BEGIN` 套 `BEGIN` 只给一句警告.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOp {
    /// `BEGIN` / `START TRANSACTION`:开一个事务.
    Begin,
    /// `COMMIT`:事务提交,改动成为正式数据.
    Commit,
    /// `ROLLBACK`:事务的改动全部撤销.
    Rollback,
}

impl TxOp {
    /// 给用户看的一行状态 / 报文里的那个词.
    pub fn as_str(&self) -> &'static str {
        match self {
            TxOp::Begin => "BEGIN",
            TxOp::Commit => "COMMIT",
            TxOp::Rollback => "ROLLBACK",
        }
    }
}

/// `CREATE DATABASE` / `DROP DATABASE` / `ALTER DATABASE` 的落点.
///
/// **为什么是一个 trait**:这几条语句改的是**服务目录**(目录树,`catalog.json`,
/// owner),而语句层只认识符号表.于是这里留一个最小的口子,由会话那一层
/// (`cmd` 的 `Session`,背后是 `usr::service::{ServiceDir, Database}`)实现;
/// 内存模式(`ydbms` 那个实验终端,以及各处的测试)用 [`NoDatabases`]
/// **明确拒绝**这些语句--不是静默无操作,也不是假装成功.
///
/// 返回值是**给用户看的一行状态**(例:`库 filmdb 已建(owner bob)`),
/// 语句层把它包成 [`Outcome::NoOp`]--语句层不该知道 owner 是谁.
pub trait Databases {
    /// 建库,返回一行状态.
    fn create_database(&mut self, name: &str) -> Result<String>;

    /// 删库,返回一行状态.
    fn drop_database(&mut self, name: &str, if_exists: bool) -> Result<String>;

    /// 记下库级的默认 `search_path`(`ALTER DATABASE ... SET search_path ...`).
    ///
    /// `path` 为空表示**清掉**这个默认值(`RESET` / `TO DEFAULT`).名字与取值都已
    /// 在语句层解好(那是 SQL 语法的事),这里只管"记到哪儿".
    fn set_database_search_path(&mut self, name: &str, path: &[String]) -> Result<String>;
}

/// 没有服务目录时的 [`Databases`]:这些语句都**明确拒绝**.
///
/// 内存模式的 `ydbms` 与所有单元测试都走它.之所以不"接受但记下来":库不在任何人
/// 的磁盘上,假装建了会让脚本在换到 `ysql` 时行为完全不同.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDatabases;

impl Databases for NoDatabases {
    fn create_database(&mut self, name: &str) -> Result<String> {
        Err(DbError::Denied {
            message: format!("{}", tr!("ir.stmt.001", arg0 = 
                name
            )),
        })
    }

    fn drop_database(&mut self, name: &str, _if_exists: bool) -> Result<String> {
        Err(DbError::Denied {
            message: format!("{}", tr!("ir.stmt.002", arg0 = 
                name
            )),
        })
    }

    fn set_database_search_path(&mut self, name: &str, _path: &[String]) -> Result<String> {
        Err(DbError::Denied {
            message: format!("{}", tr!("ir.stmt.003", arg0 = 
                name
            )),
        })
    }
}

/// 把一个值按目标列的类型**真的转一下**.
///
/// 只有一条规则:`INT` → `NUMERIC`(PostgreSQL 里唯一的隐式放大).`INSERT` 那边
/// 靠"按目标列类型定型字面量"达到同样效果;`UPDATE` 的值可以是任意表达式
/// (`SET n = i`,`i` 是 INT 列),所以在这里收口.别的组合原样交给
/// [`Table::update_row`](crate::db::table::Table::update_row) 的类型校验去拒绝.
fn coerce_to_column(value: Value, ty: &DataType) -> Value {
    match (&value, ty.family()) {
        (Value::Int(i), TypeFamily::Numeric) => Value::Numeric(i.to_string()),
        _ => value,
    }
}

/// 一条语句执行完的结果.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 查询的结果表.
    Rows(PhysTab),
    /// 建表完成.
    Created {
        /// 表名.
        name: String,
        /// 见 [`Stmt::CreateTable::unenforced`].
        unenforced: Vec<String>,
        /// **真的强制**的 `CHECK` 条数(见 [`Stmt::CreateTable::checks`]).
        checks: usize,
    },
    /// 插入完成.
    Inserted {
        /// 表名.
        table: String,
        /// 实际插入的行数.
        rows: usize,
    },
    /// 更新完成.
    Updated {
        /// 表名.
        table: String,
        /// 实际改动的行数.
        rows: usize,
    },
    /// 没有结果集,也不改表内容,更不动事务的语句:schema 与 `SET`.
    ///
    /// `message` 是给用户看的一行状态(psql 风格).这些语句都是"接受但如实记录":
    /// 没有完整的 schema 系统,`message` 说的就是实际发生了什么.
    NoOp {
        /// 一行状态.
        message: String,
    },
    /// 事务控制语句跑了:`BEGIN` / `COMMIT` / `ROLLBACK`.
    ///
    /// 单独一个变体而不是 `NoOp`,因为**引擎要按它决定磁盘那一半**:`COMMIT` 之后
    /// 那些脏表要落盘,`ROLLBACK` 之后则要**丢弃**脏表标记(改动已经被撤销,写回去
    /// 就是把撤销又撤销了).
    Transaction {
        /// 这一句是哪一种.
        op: TxOp,
        /// **真的动到事务了吗**:没有事务时的 `COMMIT` / `ROLLBACK`,以及事务里
        /// 多余的 `BEGIN` 都是 `false`(那几种按 PostgreSQL 的口径只发一句警告,
        /// 什么都没撤销,也什么都没提交).
        ///
        /// 引擎靠它区分"真回滚了"与"只发了一句警告"--前者才该丢掉脏表标记.
        applied: bool,
        /// 给用户看的一行状态(`BEGIN`)或一句警告(已经在事务里 / 没有事务).
        message: String,
    },
}

/// 一条语句的执行记录.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StmtResult {
    /// 在脚本里的序号(从 1 开始).
    pub index: usize,
    /// 语句原文,已压成一行,已截断.
    pub text: String,
    /// 这条语句的成败.
    pub outcome: Result<Outcome>,
}

/// 在符号表上执行一条语句,**没有服务目录**(见 [`execute_stmt_with`]).
///
/// 内存模式与绝大多数调用方走它:`CREATE DATABASE` / `DROP DATABASE` 由
/// [`NoDatabases`] 明确拒绝.
pub fn execute_stmt(stmt: &Stmt, symbols: &mut SymbolTable) -> Result<Outcome> {
    execute_stmt_with(stmt, symbols, &mut NoDatabases)
}

/// 在符号表上执行一条语句,`CREATE DATABASE` / `DROP DATABASE` / `ALTER DATABASE`
/// 交给 `dbs`.
///
/// # 每条语句都是一个隐式事务(4.1)
///
/// 没有显式事务时,这一句自己就是一个事务:**成功即提交,失败即回滚**.于是
/// `INSERT INTO t VALUES (1), (2), (3)` 在第三行出错时不会留下前两行,而
/// `UPDATE` 改到一半失败也不会留下半张改过的表--这就是语句级原子性.
///
/// 有事务时(用户 `BEGIN` 过,或者外面已经开了一个)原样执行,提交点归那个事务.
/// `BEGIN` 自己也是"一条语句",但它会把隐式事务**升级**成显式,所以它不会被自己
/// 那个隐式事务提交掉.
///
/// 会改东西的是:[`Stmt::CreateTable`] (往符号表里加表),[`Stmt::Insert`] /
/// [`Stmt::Update`] (改表内容),几条 schema / `SET` 语句(改的是**名字空间**,
/// 不是数据),以及数据库那三条(改的是**服务目录**,本函数不碰,整件事在 `dbs`
/// 那边).前两类都由符号表的撤销记录兜住.
pub fn execute_stmt_with(
    stmt: &Stmt,
    symbols: &mut SymbolTable,
    dbs: &mut dyn Databases,
) -> Result<Outcome> {
    let implicit = !symbols.in_transaction();
    if implicit {
        symbols.begin_implicit();
    }

    let outcome = run_stmt(stmt, symbols, dbs);

    // 连事务都还是隐式的时候,这条语句就到此为止:成功丢掉撤销记录(改动成为正式
    // 数据),失败盖回快照.事务已经变成显式(这句是 `BEGIN`)就留给 `COMMIT`.
    match (&outcome, symbols.tx_kind()) {
        (Ok(_), Some(TxKind::Implicit)) => symbols.commit_implicit(),
        (Err(_), Some(TxKind::Implicit)) => symbols.rollback_implicit(),
        _ => {}
    }

    outcome
}

/// 一条语句的实际执行([`execute_stmt_with`] 去掉隐式事务那一层).
fn run_stmt(stmt: &Stmt, symbols: &mut SymbolTable, dbs: &mut dyn Databases) -> Result<Outcome> {
    match stmt {
        Stmt::Query(plan) => Ok(Outcome::Rows(exec::execute(plan, symbols)?)),

        Stmt::CreateTable {
            name,
            columns,
            if_not_exists,
            unenforced,
            checks,
        } => {
            // 真实的名字由符号表定(`search_path` 决定要不要加前缀),报告的,
            // 判重的都是**它**--不然 `CREATE TABLE movies` 会说成 `movies`,
            // 而表其实叫 `study.movies`.
            let name = symbols.effective_name(name);

            // 表名就是磁盘上的文件名主干(见 `db::table::is_file_stem_safe`),
            // 所以**建的时候就**拒掉带路径的名字:等到落盘才报错就已经晚了一步
            // (建表在内存里已经成功,用户会以为没事).内存模式一并拒,免得同一句
            // SQL 换到 `ysql` 才炸.
            if !is_file_stem_safe(&name) {
                return Err(DbError::Denied {
                    message: format!("{}", tr!("ir.stmt.004", arg0 = 
                        name
                    )),
                });
            }

            if symbols.lookup(&name).is_some() {
                if *if_not_exists {
                    // `IF NOT EXISTS` 且已经有了:视为满足要求,不报错.
                    return Ok(Outcome::Created {
                        name,
                        unenforced: unenforced.clone(),
                        checks: checks.len(),
                    });
                }
                return Err(DbError::DuplicateSymbol { name });
            }

            let shape = ColumnsShape::new(columns.clone());
            let table = PhysTab::new(&name, &shape);
            // 有 CHECK 就包一层:所有写路径(INSERT / UPDATE)都过约束
            if checks.is_empty() {
                symbols.define(name.clone(), table)?;
            } else {
                symbols.define(name.clone(), CheckedTable::new(table, checks.clone()))?;
            }

            Ok(Outcome::Created {
                name,
                unenforced: unenforced.clone(),
                checks: checks.len(),
            })
        }

        Stmt::Insert { table, rows } => {
            // 先把每一行算出来(值表达式没有输入行),再逐行落库:
            // 索引要的键必须在行搬进表**之前**取出来,所以不能一边借表一边算.
            let mut values: Vec<Row> = Vec::with_capacity(rows.len());
            for programs in rows {
                let mut row = Row::with_capacity(programs.len());
                for prog in programs {
                    let scalar = prog.eval(&[]).map_err(|e| e.into_db_error(None))?;
                    let value = scalar.to_value().ok_or_else(|| DbError::IrRuntime {
                        node: None,
                        message: format!("{}", tr!("ir.stmt.005", arg0 =  scalar)),
                    })?;
                    row.push(value);
                }
                values.push(row);
            }

            // 报告里用**表自己的名字**(生效名,可能带 schema 前缀):脏表标记与
            // 落盘都按它来,`INSERT INTO movies`(`search_path` 是 `study`)要报
            // `study.movies`--不然提交点按 `movies` 去精确取表会取不到(见
            // `SymbolTable::relation_exact`).值都算完了再解名字,错误顺序不变.
            let effective = symbols
                .relation(table)
                .map(|relation| relation.name().to_string())
                .unwrap_or_else(|| table.clone());

            let mut inserted = 0usize;
            for row in values {
                // 索引要的键先取出来(行马上要搬进表里了)
                let keys = symbols.index_keys_of(table, &row);
                // 走 Table::insert,于是元数与类型校验都被复用.
                let rid = symbols
                    .relation_mut(table)
                    .ok_or_else(|| DbError::UnknownSymbol {
                        name: table.clone(),
                    })?
                    .insert(row)?;
                symbols.maintain_indexes(keys, rid);
                inserted += 1;
            }

            Ok(Outcome::Inserted {
                table: effective,
                rows: inserted,
            })
        }

        Stmt::Update {
            table,
            sets,
            filter,
        } => {
            // 同 `INSERT`:报告(以及提交点的脏表标记)用表自己的生效名
            let target = symbols
                .relation(table)
                .ok_or_else(|| DbError::UnknownSymbol {
                    name: table.clone(),
                })?;
            let effective = target.name().to_string();
            let nrows = target.nrows();
            let columns: Vec<Column> = symbols
                .columns(table)
                .ok_or_else(|| DbError::UnknownSymbol {
                    name: table.clone(),
                })?
                .to_vec();

            // **先**让这张表上的索引全部失效:万一中途出错(约束,短读),
            // 已经改过的行也不会留下一份错的索引--宁可下次重建一遍
            symbols.invalidate_indexes(table);

            let mut updated = 0usize;
            for r in 0..nrows {
                // 读旧行:所有 `SET` 表达式都在它上面求值
                let row = symbols
                    .relation(table)
                    .and_then(|t| t.row(r))
                    .ok_or_else(|| DbError::IrRuntime {
                        node: None,
                        message: format!("{}", tr!("ir.stmt.006", arg0 =  table, arg1 =  r)),
                    })?;

                if let Some(prog) = filter {
                    // 只有确定的真才改:UNKNOWN 与 FALSE 一样跳过
                    let truth = prog.eval_truth(&row).map_err(|e| e.into_db_error(None))?;
                    if !truth.is_true() {
                        continue;
                    }
                }

                let mut new_row = row.clone();
                for (index, prog) in sets {
                    let value = prog
                        .eval(&row)
                        .map_err(|e| e.into_db_error(None))?
                        .to_value()
                        .ok_or_else(|| DbError::IrRuntime {
                            node: None,
                            message: tr!("ir.stmt.007").into_owned(),
                        })?;
                    new_row[*index] = coerce_to_column(value, &columns[*index].ty);
                }

                // 整行一次落库:`Table::update_row` 管 typmod / NOT NULL,
                // `CheckedTable` 管 CHECK -- 约束看到的是**最终**那一行
                symbols
                    .relation_mut(table)
                    .ok_or_else(|| DbError::UnknownSymbol {
                        name: table.clone(),
                    })?
                    .update_row(r, new_row)?;
                updated += 1;
            }

            Ok(Outcome::Updated {
                table: effective,
                rows: updated,
            })
        }

        Stmt::CreateIndex {
            name,
            table,
            column,
            if_not_exists,
        } => {
            symbols.create_index(name, table, *column, *if_not_exists)?;

            // 报告里写清"建在哪张表的哪一列,多少个键"--索引是个真东西,不是记号
            let column_name = symbols
                .columns(table)
                .and_then(|columns| columns.get(*column))
                .map(|column| column.name.clone())
                .unwrap_or_else(|| column.to_string());
            let keys = symbols
                .index_of(table, *column)
                .and_then(|slot| slot.nkeys())
                .unwrap_or(0);

            Ok(Outcome::NoOp {
                message: format!("{}", tr!("ir.stmt.008", arg0 = 
                    symbols.effective_name(table), arg1 = 
                    column_name, arg2 = 
                    keys
                )),
            })
        }

        Stmt::DropIndex { name, if_exists } => {
            let dropped = symbols.drop_index(name, *if_exists)?;
            Ok(Outcome::NoOp {
                message: format!("{}", tr!("ir.stmt.009", arg0 =  dropped)),
            })
        }

        // 事务控制:**真的做**(4.1).三种各自的"做不到"都照 PostgreSQL 的口径
        // 只发一句警告,不当错误--脚本里 `BEGIN` 多写一次不该让整个装载退出 1.
        Stmt::Transaction(op) => {
            let (applied, message) = match op {
                TxOp::Begin => {
                    if symbols.tx_kind() == Some(TxKind::Explicit) {
                        // 与 PostgreSQL 同口径:不嵌套,不报错,只提醒一句
                        (
                            false,
                            tr!("ir.stmt.010").into_owned(),
                        )
                    } else {
                        // 隐式事务在这里被升级成显式:报文仍然是干净的一句 `BEGIN`
                        symbols.begin();
                        (true, TxOp::Begin.as_str().to_string())
                    }
                }
                TxOp::Commit => {
                    if symbols.commit() {
                        (true, TxOp::Commit.as_str().to_string())
                    } else {
                        (
                            false,
                            tr!("ir.stmt.011").into_owned(),
                        )
                    }
                }
                TxOp::Rollback => {
                    if symbols.rollback() {
                        (true, TxOp::Rollback.as_str().to_string())
                    } else {
                        (
                            false,
                            tr!("ir.stmt.012").into_owned(),
                        )
                    }
                }
            };
            Ok(Outcome::Transaction {
                op: *op,
                applied,
                message,
            })
        }

        Stmt::CreateSchema {
            name,
            if_not_exists,
        } => {
            symbols.create_schema(name, *if_not_exists)?;
            Ok(Outcome::NoOp {
                message: format!("{}", tr!("ir.stmt.013", arg0 =  name)),
            })
        }

        Stmt::DropSchema {
            name,
            if_exists,
            cascade,
        } => {
            let dropped = symbols.drop_schema(name, *if_exists, *cascade)?;
            Ok(Outcome::NoOp {
                message: format!("{}", tr!("ir.stmt.014", arg0 =  name, arg1 =  dropped)),
            })
        }

        Stmt::SetSearchPath { path } => {
            symbols.set_search_path(path.clone());
            Ok(Outcome::NoOp {
                message: format!("search_path = {}", path.join(", ")),
            })
        }

        // 服务目录那一层的事:一个字的文件系统知识都不在这边.
        //
        // 事务里一律拒绝:它们动的是**服务目录**(建目录,删目录,写 `catalog.json`),
        // 而事务的撤销记录只管符号表--回滚一个建好的库目录做不到,那就不如不做
        // (PostgreSQL 也是"不能在事务块里跑").
        Stmt::CreateDatabase { name } => {
            reject_in_transaction(symbols, "CREATE DATABASE")?;
            Ok(Outcome::NoOp {
                message: dbs.create_database(name)?,
            })
        }

        Stmt::DropDatabase { name, if_exists } => {
            reject_in_transaction(symbols, "DROP DATABASE")?;
            Ok(Outcome::NoOp {
                message: dbs.drop_database(name, *if_exists)?,
            })
        }

        // 库级默认值也写**服务目录**(`catalog.json`),所以与上两条一样在事务里
        // 拒绝:撤销记录只管符号表,`ROLLBACK` 盖不回一个已经改过的文件--留一个
        // 静默生效的例外,比明确拒绝坏得多.
        Stmt::AlterDatabase { name, search_path } => {
            reject_in_transaction(symbols, "ALTER DATABASE")?;
            Ok(Outcome::NoOp {
                message: dbs.set_database_search_path(name, search_path)?,
            })
        }
    }
}

/// 这几条语句在事务里一律拒绝(见调用处的理由).
///
/// 判的是**显式**事务:`execute_stmt_with` 给每条语句都开了一个隐式事务,用
/// `in_transaction()` 会把事务外的 `CREATE DATABASE` 也一起挡掉.
fn reject_in_transaction(symbols: &SymbolTable, what: &str) -> Result<()> {
    if symbols.tx_kind() == Some(TxKind::Explicit) {
        return Err(DbError::Denied {
            message: format!("{}", tr!("ir.stmt.015", arg0 = 
                what
            )),
        });
    }
    Ok(())
}

/// 解析整段 SQL(可含多条语句),逐条"降级 → 执行".
///
/// 外层 `Result` 只表示"解析/切分"这一步就失败了(那时一条都没跑);每条语句
/// 自己的成败在 [`StmtResult::outcome`] 里.**一条失败不影响后面的语句**--
/// 与 psql 默认行为一致.
///
/// 每条语句都是一个隐式事务(成功即提交,失败即回滚,见 [`execute_stmt_with`]),
/// 所以失败的那一条自己不会留下半拉改动;但它**前面**成功的语句已经生效--脚本
/// 中途失败仍会留下一个建了一半的库,除非整段包在 `BEGIN; ... COMMIT;` 里.
///
/// 想要"一条一条自己驱动"(计时,遇错就停)用 [`Script`].
pub fn run_script(sql: &str, symbols: &mut SymbolTable) -> Result<Vec<StmtResult>> {
    let script = Script::parse(sql)?;
    Ok((0..script.len())
        .map(|index| script.run_one(index, symbols))
        .collect())
}

/// 一份**已解析**的脚本:可以逐条降级 + 执行.
///
/// 解析与执行分开,是为了让调用方拿到"每条语句"的粒度:`\timing` 要在一条语句
/// 前后计时,`ON_ERROR_STOP` 要在第一条失败处停下.**降级仍然在每条执行时才做**
/// (后面的语句依赖前面建出来的表).
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
    /// 原文:`statement_text` 要从里面切出每条语句的文本.
    sql: String,
    /// `pg_query` 的裸语句节点.**保持私有**:`pg_query` 的类型不外泄.
    statements: Vec<RawStmt>,
}

impl Script {
    /// 解析整段 SQL.这里的 `Err` 表示**一条都没跑**.
    pub fn parse(sql: &str) -> Result<Script> {
        let parsed = pg_query::parse(sql).map_err(|e| DbError::Parse {
            message: e.to_string(),
        })?;

        Ok(Script {
            sql: sql.to_string(),
            statements: parsed.protobuf.stmts,
        })
    }

    /// 语句条数.
    pub fn len(&self) -> usize {
        self.statements.len()
    }

    /// 一条语句都没有.
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// 执行第 `index` 条(0 起),返回这条的记录.
    ///
    /// 调用方按 `0..len()` 循环,所以下标一定在界内.**失败不中止**后续语句--
    /// 要不要停由调用方决定(`ON_ERROR_STOP`).
    ///
    /// 没有服务目录的会话走它(见 [`Script::run_one_with`]).
    pub fn run_one(&self, index: usize, symbols: &mut SymbolTable) -> StmtResult {
        self.run_one_with(index, symbols, &mut NoDatabases)
    }

    /// 同 [`Script::run_one`],但把 `CREATE DATABASE` / `DROP DATABASE` 交给 `dbs`.
    pub fn run_one_with(
        &self,
        index: usize,
        symbols: &mut SymbolTable,
        dbs: &mut dyn Databases,
    ) -> StmtResult {
        let raw = &self.statements[index];

        // 逐条来:后面的语句依赖前面建出来的表,不能先全降级.
        let outcome = match raw.stmt.as_deref() {
            Some(node) => match irgen::lower_statement(node, symbols) {
                Ok(stmt) => execute_stmt_with(&stmt, symbols, dbs),
                Err(e) => Err(e),
            },
            None => Err(DbError::Parse {
                message: tr!("ir.stmt.016").into_owned(),
            }),
        };

        StmtResult {
            index: index + 1,
            text: statement_text(&self.sql, raw),
            outcome,
        }
    }
}

/// 这段 SQL 里有没有**顶层**的分号--REPL 用它判断"这一句敲完了没有".
///
/// "顶层" = 不在字符串,注释或美元引用里:
///
/// * `'...'`(`''` 是一个字面单引号)与 `"..."`(`""` 同理);
/// * `-- ...` 直到行尾;`/* ... */`,**允许嵌套**(PostgreSQL 也允许);
/// * `$$...$$` 与 `$tag$...$tag$`.
///
/// 已知不处理:`E'...'` 里的反斜杠转义.PG 默认 `standard_conforming_strings = on`,
/// 普通 `'...'` 里的反斜杠是字面量,所以只有显式写了 `E''` 才会看错.
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
            // ASCII 之外都是 UTF-8 的续字节,不会与上面的字节撞上
            _ => i += 1,
        }
    }

    false
}

/// 跳过一段 `'...'` / `"..."`,返回它后面那个字节的位置.没闭合就到结尾.
fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            // 两个引号连写 = 一个字面引号,不算结束
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

/// 跳过 `-- ...`(含行尾换行).
fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    match bytes[start..].iter().position(|b| *b == b'\n') {
        Some(offset) => start + offset + 1,
        None => bytes.len(),
    }
}

/// 跳过 `/* ... */`,**按嵌套算**(`/* a /* b */ c */` 是一整段注释).
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

/// 跳过 `$$...$$` / `$tag$...$tag$`.不是合法的美元引用就只前进一个字节
/// (`$1` 之类的参数占位符不该把它吃掉).
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> usize {
    let tag_end = match bytes[start + 1..].iter().position(|b| *b == b'$') {
        Some(offset) => start + 1 + offset,
        None => return start + 1,
    };

    // 标签只能是标识符字符(`$$` 的标签是空的)
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

/// 从原脚本里切出这条语句的原文.
///
/// 用 [`RawStmt`] 的 `stmt_location` / `stmt_len`.实测这两个字段是准的,但
/// 长度的口径会把尾随换行算进去,所以一律 `trim`;切片越界就退回"从头切到
/// 结尾"或空串,绝不 panic.
fn statement_text(sql: &str, raw: &RawStmt) -> String {
    let start = raw.stmt_location.max(0) as usize;
    let len = raw.stmt_len.max(0) as usize;

    let slice = if len == 0 {
        sql.get(start..)
    } else {
        sql.get(start..start + len).or_else(|| sql.get(start..))
    };

    // `stmt_location` 把**前导注释**也算进语句里,所以带注释的脚本里这个切片
    // 会以 `-- ...` 开头,`[N] <原文>` 就显示成注释而不是 SQL.先剥掉.
    let body = strip_leading_comments(slice.unwrap_or(""));

    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");

    truncate(&one_line, 60)
}

/// 剥掉开头的空白与 SQL 注释(`--` 行注释与 `/* */` 块注释).
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
