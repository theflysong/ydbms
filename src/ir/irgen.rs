//! AST → 线性 IR 的降级(绑定器).
//!
//! 输入是 `pg_query`(PostgreSQL 服务端的真实解析器)产出的**裸语法树**,输出是
//! [`Plan`].中间要做的正是 PostgreSQL 里 `transformStmt` 那一步的缩小版:
//!
//! * **查表**:`FROM` 里的每个名字去 [`SymbolTable`] 换出列定义;
//! * **解列名**:`x`,`f.x`,`study.people.x` 都要落到**扁平下标**上;
//! * **类型检查**:交给下游 [`Plan::analyze`] 与 [`Prog::verify`];
//! * **确认形状**:`AS` 只影响限定名匹配,不改变列名(列名保持表自己的).
//! * **挑访问路径与连接策略**:`WHERE` 里只引用一个 FROM 项的合取项下推到那
//!   张表的 `Scan.filter`(能走索引就顺手挑索引路径,见 `opt::pushdown` 的 `plan_where`);
//!   `ON` 里的"裸列 = 裸列"等值键决定这次连接走嵌套循环 / 哈希 / 索引嵌套循环
//!   (见 `opt::join`).两条都**只挑已知能对的**:挑不出
//!   来就退回首选方案,正确性不变.
//!
//! # 一条铁律:不认识的写法一律拒绝
//!
//! 凡是指向**尚未实现**能力的写法,一律**显式报 [`DbError::Unsupported`]**,
//! 绝不猜.宁可拒绝,也不要给一个安静的错误答案--`IS NULL` 就是个例子:在值
//! 模型还没有 NULL 的年代,若把它当"恒为假"执行,`died IS NULL` 会安静地返回
//! 0 行,**错得很隐蔽**,比报错危险得多.
//!
//! 仍未实现的(都在本文件里显式拒绝):CTE;`INTERSECT`/`EXCEPT`;`DISTINCT ON`;
//! 窗口函数;`GROUPING SETS`/`ROLLUP`/`CUBE`;`RIGHT`/`FULL`/`SEMI`/`ANTI` JOIN,
//! `JOIN ... USING` 与 `NATURAL JOIN`;`LATERAL`;`UPDATE`/`DELETE`/`DROP TABLE`/
//! `ALTER TABLE`;DDL 的各种变体(`CREATE VIEW`/`AS SELECT`,`ALTER DATABASE` 的
//! `RENAME`/`OWNER`...);`SAVEPOINT` 与嵌套事务(`BEGIN`/`COMMIT`/`ROLLBACK` 与
//! `ALTER DATABASE ... SET search_path` 是支持的);参数占位符;数组与位串类型.
//!
//! 角色那几条**是支持的**(`CREATE/ALTER/DROP USER`·`ROLE`·`GROUP`,
//! `GRANT/REVOKE <角色>`,`SET ROLE`/`RESET ROLE`);其中**显式拒绝**的是:
//! `CREATE ROLE` 的 `VALID UNTIL`/`CONNECTION LIMIT`/`SYSID`/`ADMIN`/`IN GROUP`,
//! `ALTER ROLE ... IN DATABASE ... SET/RESET`,`WITH ADMIN OPTION` /
//! `ADMIN OPTION FOR` / `GRANTED BY` / `CASCADE`,以及
//! `CURRENT_USER`/`SESSION_USER`/`PUBLIC` 这几种角色写法.
//!
//! # 子查询与相关引用
//!
//! 子查询就是"再开一个查询":`lower_select` 多收一个**外层作用域链**
//! (`outers`,`outers[0]` 是直接外层),解列名时沿着它往外走--解到外层就落成
//! `Step::Outer { depth, index }`,内层计划则整份装进 `SubPlan`.派生表走
//! [`PlanBuilder::splice`]:内层计划的节点直接接进外层数组,不新增算子.
//!
//! 分组查询里的相关子查询**显式拒绝**(内层链留空,引用外层会以"找不到这一列"
//! 报出来):PostgreSQL 也要求那种相关列出现在 `GROUP BY` 里,本版没有那套检查.
//!
//! # 绑定期的作用域
//!
//! 列引用解出来的是**相对算子输入 schema 的扁平下标**.`FROM a, b` 或
//! `a JOIN b` 之后,schema 是"左 ++ 右",所以 `Scope` 记着每一项的偏移量,
//! 解列名时先定位到项,再加上偏移.这个扁平 schema 必须与
//! [`Plan::analyze`] 推导出来的完全一致--若哪天不一致,`analyze` 会以
//! `IrInvalid` 报出来,而不会静默读错列.

use std::collections::{BTreeMap, BTreeSet};

use pg_query::protobuf::{
    a_const::Val, node, AConst, AExpr, AExprKind, AlterDatabaseSetStmt, AlterRoleStmt, BoolExpr,
    BoolExprType, CaseExpr, CoalesceExpr, ConstrType, CreateRoleStmt, CreateSchemaStmt, CreateStmt,
    CreatedbStmt,
    DefElem, DropBehavior, DropOwnedStmt, DropRoleStmt, DropStmt,
    DropdbStmt, FuncCall, GrantRoleStmt, InsertStmt,
    JoinExpr, JoinType, LimitOption, Node, NullTestType, ObjectType, OnCommitAction, RangeVar,
    AlterOwnerStmt, GrantStmt, GrantTargetType, IndexStmt, RenameStmt, RoleSpec, RoleSpecType,
    RoleStmtType, SelectStmt, SetOperation, SortBy,
    SortByDir,
    SortByNulls,
    SqlValueFunction, SqlValueFunctionOp, SubLink, SubLinkType, TransactionStmt,
    TransactionStmtKind, TypeName, UpdateStmt, VacuumStmt, VariableSetKind, VariableSetStmt,
};

use crate::db::table::{
    Column, DataType, DbError, Result, TypeFamily, Value,
    DEFAULT_TIMESTAMP_PRECISION,
    MAX_TIMESTAMP_PRECISION,
};
use crate::usr::grants::Privilege;
use crate::usr::{Password, PasswordAction, RoleAttrChange, RoleAttrs};

use super::checks::Check;
use super::expr::{
    unified_branch_type, BinOp, Prog, ProgBuilder, Reg, ScalarFunc, SubMode, SubPlan, UnOp,
};
use super::value::ScalarType;
use super::opt;
use super::plan::{
    passthrough_column, plan_has_outer, project_column, AggFunc, AggSpec, JoinKind, JoinStrategy,
    NodeId, Op, Plan, PlanBuilder, SortKey,
};
use super::stmt::{
    AlterRoleRequest, CreateRoleRequest, GrantRequest, GrantRoleRequest, GrantTarget, PrivilegeGrant,
    Stmt, TxOp,
};
use super::symbol::SymbolTable;

/// 解析 SQL 并降级成**恰好一条查询**.
///
/// 这是给 `EXPLAIN` / [`crate::ir::compile`] 用的:它只要查询.要跑脚本(可能
/// 含多条语句,可能含 DDL/DML)用 [`crate::ir::run_script`].
pub fn lower(sql: &str, symbols: &SymbolTable) -> Result<Plan> {
    let parsed = pg_query::parse(sql).map_err(|e| DbError::Parse {
        message: e.to_string(),
    })?;

    if parsed.protobuf.stmts.is_empty() {
        return Err(DbError::Parse {
            message: tr!("ir.irgen.001").into_owned(),
        });
    }
    if parsed.protobuf.stmts.len() > 1 {
        return Err(unsupported(format!("{}", tr!("ir.irgen.002", arg0 = 
            parsed.protobuf.stmts.len()
        ))));
    }

    let node = parsed.protobuf.stmts[0]
        .stmt
        .as_deref()
        .ok_or_else(|| DbError::Parse {
            message: tr!("ir.irgen.003").into_owned(),
        })?;

    match lower_statement(node, symbols)? {
        Stmt::Query(plan) => Ok(plan),
        _ => Err(unsupported(
            tr!("ir.irgen.004").into_owned(),
        )),
    }
}

/// 把一个已经解析好的语句节点降级成 [`Stmt`].
pub fn lower_statement(stmt: &Node, symbols: &SymbolTable) -> Result<Stmt> {
    match &stmt.node {
        Some(node::Node::SelectStmt(select)) => {
            Ok(Stmt::Query(lower_select(select, symbols, &[])?))
        }
        Some(node::Node::CreateStmt(create)) => lower_create_table(create, symbols),
        Some(node::Node::InsertStmt(insert)) => lower_insert(insert, symbols),
        Some(node::Node::UpdateStmt(update)) => lower_update(update, symbols),
        Some(node::Node::DeleteStmt(_)) => Err(unsupported("DELETE")),
        Some(node::Node::DropStmt(drop)) => lower_drop(drop),
        Some(node::Node::AlterTableStmt(_)) => Err(unsupported("ALTER TABLE")),
        Some(node::Node::CreateTableAsStmt(_)) => Err(unsupported("CREATE TABLE ... AS SELECT")),
        Some(node::Node::IndexStmt(index)) => lower_create_index(index, symbols),
        Some(node::Node::ViewStmt(_)) => Err(unsupported("CREATE VIEW")),
        Some(node::Node::CreateSchemaStmt(schema)) => lower_create_schema(schema),
        Some(node::Node::CreatedbStmt(db)) => lower_create_database(db),
        Some(node::Node::DropdbStmt(db)) => lower_drop_database(db),
        Some(node::Node::AlterDatabaseSetStmt(db)) => lower_alter_database_set(db),
        Some(node::Node::AlterDatabaseStmt(_)) | Some(node::Node::AlterDatabaseRefreshCollStmt(_)) => {
            Err(unsupported(tr!("ir.irgen.005").into_owned()))
        }
        Some(node::Node::TransactionStmt(tx)) => lower_transaction(tx),
        Some(node::Node::VariableSetStmt(set)) => lower_set(set),
        Some(node::Node::VacuumStmt(vacuum)) => lower_vacuum(vacuum),
        Some(node::Node::CreateRoleStmt(role)) => lower_create_role(role),
        Some(node::Node::AlterRoleStmt(role)) => lower_alter_role(role),
        Some(node::Node::AlterRoleSetStmt(_)) => Err(unsupported("ALTER ROLE ... IN DATABASE")),
        Some(node::Node::DropRoleStmt(drop)) => lower_drop_role(drop),
        Some(node::Node::GrantRoleStmt(grant)) => lower_grant_role(grant),
        Some(node::Node::RenameStmt(rename)) => lower_rename(rename),
        Some(node::Node::GrantStmt(grant)) => lower_grant(grant, symbols),
        Some(node::Node::DropOwnedStmt(drop)) => lower_drop_owned(drop),
        Some(node::Node::AlterOwnerStmt(owner)) => lower_alter_owner(owner),
        _ => Err(unsupported(tr!("ir.irgen.006").into_owned())),
    }
}

// --------------------------------------------------------------- 集合运算

/// `SELECT ... UNION [ALL] SELECT ...`.
///
/// 两支各自降级成**自足**的 `Plan`,再用 `Op::Union` 接起来([`PlanBuilder::splice`]
/// 把两支的节点接进同一个数组).`UNION` 去重,`UNION ALL` 不去--都在执行期做
/// (见 `exec` 的 `UnionSource`).
///
/// `INTERSECT` / `EXCEPT` 继续保持显式拒绝:它们要的"另一支里的键"本版没有.
fn lower_setop(
    select: &SelectStmt,
    set_op: SetOperation,
    symbols: &SymbolTable,
    outers: &[Scope<'_>],
) -> Result<Plan> {
    let all = match set_op {
        SetOperation::SetopUnion => select.all,
        SetOperation::SetopIntersect => return Err(unsupported(tr!("ir.irgen.007").into_owned())),
        SetOperation::SetopExcept => return Err(unsupported(tr!("ir.irgen.008").into_owned())),
        _ => return Err(unsupported(tr!("ir.irgen.009").into_owned())),
    };

    if select.with_clause.is_some() {
        return Err(unsupported(tr!("ir.irgen.010").into_owned()));
    }

    // `larg` / `rarg` 直接就是 `SelectStmt`(不是 `Node`):嵌套的 `UNION` 递归地
    // 落在自己的 `lower_select` 里.
    let left = select
        .larg
        .as_deref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.011").into_owned()))?;
    let right = select
        .rarg
        .as_deref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.012").into_owned()))?;

    let left_plan = lower_select(left, symbols, outers)?;
    let right_plan = lower_select(right, symbols, outers)?;
    // 输出 schema 取左支的(列名也是):浅层分析就够,外层链的校验留给外层 `analyze`
    let schema = left_plan.analyze_shallow(symbols)?.schemas[left_plan.root].clone();

    let mut b = PlanBuilder::new();
    let left_id = b.splice(left_plan)?;
    let right_id = b.splice(right_plan)?;
    let mut current = b.union(left_id, right_id, all)?;

    // `... UNION ... ORDER BY 1 LIMIT 3`:排序键按**并集的输出 schema** 解
    if !select.sort_clause.is_empty() || select.limit_count.is_some() {
        let scope = Scope::from_range(String::new(), None, schema, None, symbols);
        let keys = lower_sort_keys(&select.sort_clause, &scope, true)?;
        if !keys.is_empty() {
            current = b.sort(current, keys)?;
        }
        let (limit, offset) = lower_limit(select)?;
        if limit.is_some() || offset > 0 {
            current = b.limit(current, limit, offset)?;
        }
    }

    b.finish(current)
}

// ------------------------------------------------- schema / 事务 / SET(1.5)

/// 事务控制:`BEGIN` / `START TRANSACTION` → [`TxOp::Begin`],`COMMIT` →
/// [`TxOp::Commit`],`ROLLBACK` → [`TxOp::Rollback`].
///
/// 三种都是**真的执行**(撤销记录在符号表里,见 [`SymbolTable`]).选项与
/// `SAVEPOINT` / 两阶段提交继续显式拒绝:本版没有它们,接受下来只能假装.
fn lower_transaction(tx: &TransactionStmt) -> Result<Stmt> {
    if !tx.options.is_empty() || !tx.savepoint_name.is_empty() || !tx.gid.is_empty() || tx.chain {
        return Err(unsupported(tr!("ir.irgen.013").into_owned()));
    }

    let kind =
        TransactionStmtKind::try_from(tx.kind).unwrap_or(TransactionStmtKind::Undefined);
    match kind {
        TransactionStmtKind::TransStmtBegin | TransactionStmtKind::TransStmtStart => {
            Ok(Stmt::Transaction(TxOp::Begin))
        }
        TransactionStmtKind::TransStmtCommit => Ok(Stmt::Transaction(TxOp::Commit)),
        TransactionStmtKind::TransStmtRollback => Ok(Stmt::Transaction(TxOp::Rollback)),
        other => Err(unsupported(format!("{}", tr!("ir.irgen.014", arg0 =  other.as_str_name())))),
    }
}

/// `ALTER DATABASE name SET search_path TO ...`:库级默认值(见
/// [`Server::set_database_search_path`]).
///
/// 只认 `search_path` 这一个变量--它是库元数据里唯一有语义的东西,别的变量本版
/// 没有会话默认值可言,接受下来等于假装.`RESET search_path` 与
/// `SET search_path TO DEFAULT` 解成**清掉**(空路径).
fn lower_alter_database_set(db: &AlterDatabaseSetStmt) -> Result<Stmt> {
    if db.dbname.is_empty() {
        return Err(unsupported(tr!("ir.irgen.015").into_owned()));
    }
    let set = db
        .setstmt
        .as_ref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.016").into_owned()))?;

    let kind = VariableSetKind::try_from(set.kind).unwrap_or(VariableSetKind::Undefined);
    if !set.name.eq_ignore_ascii_case("search_path") {
        return Err(unsupported(format!("{}", tr!("ir.irgen.017", arg0 = 
            set.name
        ))));
    }

    let search_path = match kind {
        VariableSetKind::VarSetValue => parse_search_path(set)?,
        // `RESET search_path` 与 `SET search_path TO DEFAULT`:把这个默认值清掉
        // (空 = 不加前缀,与过去一样)
        VariableSetKind::VarSetDefault | VariableSetKind::VarReset => Vec::new(),
        _ => {
            return Err(unsupported(format!("{}", tr!("ir.irgen.018", arg0 = 
                set.name
            ))))
        }
    };

    Ok(Stmt::AlterDatabase {
        name: db.dbname.clone(),
        search_path,
    })
}

/// 解出 `search_path` 的取值(`SET` 与 `ALTER DATABASE ... SET` 共用的那一份).
///
/// 多值时语法树给的是"一串 `A_Const`";单值有时会被包成一层 `List`.
fn parse_search_path(set: &VariableSetStmt) -> Result<Vec<String>> {
    match set.args.as_slice() {
        [only] => match &only.node {
            Some(node::Node::List(list)) => list
                .items
                .iter()
                .map(const_string)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| unsupported(tr!("ir.irgen.019").into_owned())),
            _ => Ok(vec![const_string(only)
                .ok_or_else(|| unsupported(tr!("ir.irgen.020").into_owned()))?]),
        },
        args => args
            .iter()
            .map(const_string)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| unsupported(tr!("ir.irgen.021").into_owned())),
    }
}

/// `CREATE DATABASE name`:只留名字,建目录 / 写 `catalog.json` 是 [`Server`] 的事.
///
/// [`Server`]: super::stmt::Server
fn lower_create_database(db: &CreatedbStmt) -> Result<Stmt> {
    if !db.options.is_empty() {
        return Err(unsupported(
            tr!("ir.irgen.022").into_owned(),
        ));
    }
    if db.dbname.is_empty() {
        return Err(unsupported(tr!("ir.irgen.023").into_owned()));
    }

    Ok(Stmt::CreateDatabase {
        name: db.dbname.clone(),
    })
}

/// `DROP DATABASE [IF EXISTS] name`.
///
/// [`Server`]: super::stmt::Server
fn lower_drop_database(db: &DropdbStmt) -> Result<Stmt> {
    if !db.options.is_empty() {
        return Err(unsupported(tr!("ir.irgen.024").into_owned()));
    }
    if db.dbname.is_empty() {
        return Err(unsupported(tr!("ir.irgen.025").into_owned()));
    }

    Ok(Stmt::DropDatabase {
        name: db.dbname.clone(),
        if_exists: db.missing_ok,
    })
}

/// `CREATE SCHEMA name`:只登记名字(限定名本来就能用,见 [`crate::ir::symbol`]).
fn lower_create_schema(schema: &CreateSchemaStmt) -> Result<Stmt> {
    if schema.authrole.is_some() {
        return Err(unsupported("CREATE SCHEMA ... AUTHORIZATION"));
    }
    if !schema.schema_elts.is_empty() {
        return Err(unsupported(tr!("ir.irgen.026").into_owned()));
    }
    if schema.schemaname.is_empty() {
        return Err(unsupported(tr!("ir.irgen.027").into_owned()));
    }

    Ok(Stmt::CreateSchema {
        name: schema.schemaname.clone(),
        if_not_exists: schema.if_not_exists,
    })
}

/// `DROP`:只认 `DROP SCHEMA` 与 `DROP INDEX`,别的仍然整个拒绝.
///
/// schema 的"删除"在本版就是**清掉该前缀的符号**(一个会话一个进程,
/// 没有别的地方能引用它们);索引的删除就是把目录里那一条去掉.
fn lower_drop(drop: &DropStmt) -> Result<Stmt> {
    let object = ObjectType::try_from(drop.remove_type).unwrap_or(ObjectType::Undefined);

    if object == ObjectType::ObjectIndex {
        if drop.concurrent {
            return Err(unsupported("DROP INDEX CONCURRENTLY"));
        }
        // `CASCADE` / `RESTRICT` 都接受:本版没有任何东西依赖索引
        let names: Vec<String> = drop.objects.iter().filter_map(object_name).collect();
        let name = match names.as_slice() {
            [one] => one.clone(),
            [] => return Err(unsupported(tr!("ir.irgen.028").into_owned())),
            _ => return Err(unsupported(tr!("ir.irgen.029").into_owned())),
        };
        return Ok(Stmt::DropIndex {
            name,
            if_exists: drop.missing_ok,
        });
    }

    if object != ObjectType::ObjectSchema {
        let what = match object {
            ObjectType::ObjectTable => "DROP TABLE".to_string(),
            ObjectType::ObjectView => "DROP VIEW".to_string(),
            ObjectType::ObjectSequence => "DROP SEQUENCE".to_string(),
            ObjectType::ObjectType => "DROP TYPE".to_string(),
            _ => tr!("ir.irgen.030").into_owned(),
        };
        return Err(unsupported(what));
    }
    if drop.concurrent {
        return Err(unsupported("DROP SCHEMA CONCURRENTLY"));
    }

    let cascade = DropBehavior::try_from(drop.behavior).unwrap_or(DropBehavior::Undefined)
        == DropBehavior::DropCascade;

    // 语法树里每个对象是"`.` 分段的 `List`";单段的实测也可能是裸 `String`
    let names: Vec<String> = drop.objects.iter().filter_map(object_name).collect();
    let name = match names.as_slice() {
        [one] => one.clone(),
        [] => return Err(unsupported(tr!("ir.irgen.031").into_owned())),
        _ => return Err(unsupported(tr!("ir.irgen.032").into_owned())),
    };

    Ok(Stmt::DropSchema {
        name,
        if_exists: drop.missing_ok,
        cascade,
    })
}

/// 一个"对象名"节点 → 它的文本(`DROP` 的 `objects` 列表里那些).
///
/// 限定名是 `List[String, String...]`;单段名实测直接是个 `String`.两种都认,
/// 别的给 `None`(调用方会把它当成"没有名字"报出来).
fn object_name(node: &Node) -> Option<String> {
    match &node.node {
        Some(node::Node::List(list)) => Some(ref_parts(&list.items)?.join(".")),
        Some(node::Node::String(s)) => Some(s.sval.clone()),
        _ => None,
    }
}

/// `SET`:只认 `SET search_path TO ...` 与 `SET ROLE ...`,别的变量一律拒绝.
///
/// `search_path` **不是**无操作的:`filmdb.sql` 的建表写的是非限定名,而测试查的
/// 是 `study.movies`--不作前缀那些表就永远不存在(见 [`crate::ir::symbol`]).
///
/// `SET ROLE` 在语法树里就是一个"变量"(PostgreSQL 的 `role` 是个 GUC 式的开关):
/// 解出目标名交出去;`SET ROLE NONE` / `DEFAULT` 与 `RESET ROLE` 都解成 `None`
/// (=回到会话用户).`SET SESSION AUTHORIZATION` 走下面那条"不认识的变量"拒绝.
fn lower_set(set: &VariableSetStmt) -> Result<Stmt> {
    let kind = VariableSetKind::try_from(set.kind).unwrap_or(VariableSetKind::Undefined);

    if set.name.eq_ignore_ascii_case("role") {
        return match kind {
            VariableSetKind::VarSetValue => {
                let text = set_name_string(set)
                    .ok_or_else(|| unsupported(tr!("ir.irgen.205").into_owned()))?;
                let target = match text.to_ascii_lowercase().as_str() {
                    "none" | "default" => None,
                    _ => Some(text),
                };
                Ok(Stmt::SetRole(target))
            }
            VariableSetKind::VarSetDefault | VariableSetKind::VarReset => Ok(Stmt::SetRole(None)),
            _ => Err(unsupported(format!("{}", tr!("ir.irgen.205")))),
        };
    }

    if kind != VariableSetKind::VarSetValue {
        return Err(unsupported(format!("{}", tr!("ir.irgen.033", arg0 =  set.name))));
    }
    if !set.name.eq_ignore_ascii_case("search_path") {
        return Err(unsupported(format!("{}", tr!("ir.irgen.034", arg0 =  set.name))));
    }

    // 多值时语法树给的是"一串 `A_Const`";单值有时会被包成一层 `List`
    let names = parse_search_path(set)?;

    Ok(Stmt::SetSearchPath { path: names })
}

/// `SET ROLE` 的取值:恰好一个字符串字面量.
fn set_name_string(set: &VariableSetStmt) -> Option<String> {
    match set.args.as_slice() {
        [only] => const_string(only),
        _ => None,
    }
}

/// `VACUUM` / `ANALYZE`:收下所有变体,交给执行期**空转**.
///
/// 为什么这一版只能空转:统计信息还没有(基数估算与代价模型是 3.4 的待办),空间回收
/// 也不需要(`UPDATE` 是原地替换,没有死元组).收下的是:
///
/// * `VACUUM` / `VACUUM ANALYZE` / `VACUUM (ANALYZE)` / `ANALYZE [VERBOSE]`;
/// * **选项列表一律接受并忽略**(`FULL`,`VERBOSE`,`FREEZE` 这些全是空转);
/// * 不带表(整库)与多表列表都收,表名原样交给执行期去认
///   (`Stmt::Vacuum::tables`)--`VACUUM typos` 不能安静地成功.
///
/// 两处**有意**的偏差:选项的取值不解释(反正都是空转),事务块里**不**拒绝
/// (PostgreSQL 的 `VACUUM` 不能在事务里跑).两条都写在 `src/ir/AGENTS.md`.
fn lower_vacuum(vacuum: &VacuumStmt) -> Result<Stmt> {
    let what = match vacuum.is_vacuumcmd {
        true => "VACUUM",
        false => "ANALYZE",
    };

    let mut tables = Vec::with_capacity(vacuum.rels.len());
    for rel in &vacuum.rels {
        let relation = match &rel.node {
            Some(node::Node::VacuumRelation(entry)) => entry.relation.as_ref(),
            // 这一支里只装 `VacuumRelation`;万一不是,照旧显式拒绝
            _ => return Err(unsupported(tr!("ir.irgen.006").into_owned())),
        };
        // 不带表的整库形式是 `rels` 空,`relation` 不会是 `None`
        if let Some(rv) = relation {
            tables.push(qualified_name(rv)?);
        }
    }

    Ok(Stmt::Vacuum { what, tables })
}

// --------------------------------------------------------------- 角色(6.4)

/// `CREATE USER` / `CREATE ROLE` / `CREATE GROUP`.
///
/// 三者的差别只有**默认属性**(`USER` 默认能登录)与报文里的那个词,别的选项都一样:
/// `PASSWORD` / `SUPERUSER` / `CREATEDB` / `CREATEROLE` / `INHERIT` / `IN ROLE`.
/// 其余的(`VALID UNTIL`,`CONNECTION LIMIT`,`BYPASSRLS`,`REPLICATION`,`SYSID`,
/// `ADMIN`,`IN GROUP`...)一律**显式拒绝**--写进来了却不当回事,比报错坏.
fn lower_create_role(role: &CreateRoleStmt) -> Result<Stmt> {
    let kind = RoleStmtType::try_from(role.stmt_type).unwrap_or(RoleStmtType::Undefined);
    let (what, mut attrs) = match kind {
        RoleStmtType::RolestmtUser => ("CREATE USER", RoleAttrs::user()),
        RoleStmtType::RolestmtRole => ("CREATE ROLE", RoleAttrs::role()),
        RoleStmtType::RolestmtGroup => ("CREATE GROUP", RoleAttrs::role()),
        _ => {
            return Err(unsupported(format!("{}", tr!("ir.irgen.196", arg0 = 
                kind.as_str_name()
            ))))
        }
    };
    if role.role.trim().is_empty() {
        return Err(unsupported(format!("{}", tr!("ir.irgen.201"))));
    }

    let mut password = None;
    let mut add_to = Vec::new();
    for option in &role.options {
        let elem = match &option.node {
            Some(node::Node::DefElem(elem)) => elem,
            _ => {
                return Err(unsupported(format!("{}", tr!("ir.irgen.198", arg0 = 
                    what
                ))))
            }
        };

        match elem.defname.as_str() {
            "password" => {
                // `PASSWORD NULL` 在语法树里是个空值节点:与不写同义(没有口令)
                password = match def_string(elem) {
                    Some(text) => {
                        if text.is_empty() {
                            return Err(unsupported(format!("{}", tr!("ir.irgen.197"))));
                        }
                        Some(Password::new(text))
                    }
                    None => None,
                };
            }
            "superuser" => {
                attrs.admin = def_bool(elem).ok_or_else(|| {
                    unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  elem.defname)))
                })?
            }
            "createdb" => {
                attrs.create_db = def_bool(elem).ok_or_else(|| {
                    unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  elem.defname)))
                })?
            }
            "createrole" => {
                attrs.create_role = def_bool(elem).ok_or_else(|| {
                    unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  elem.defname)))
                })?
            }
            "inherit" => {
                attrs.inherit = def_bool(elem).ok_or_else(|| {
                    unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  elem.defname)))
                })?
            }
            // `LOGIN` / `NOLOGIN` 在语法树里就是 `canlogin` 这一条
            "canlogin" => {
                attrs.login = def_bool(elem).ok_or_else(|| {
                    unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  elem.defname)))
                })?
            }
            "addroleto" => add_to.extend(def_roles(elem)?),
            other => {
                return Err(unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  other))))
            }
        }
    }

    Ok(Stmt::CreateRole(CreateRoleRequest {
        what,
        name: role.role.clone(),
        password,
        attrs,
        add_to,
    }))
}

/// `ALTER USER` / `ALTER ROLE`:改属性,换口令,改名.
///
/// **`ALTER USER` 与 `ALTER ROLE` 在语法树里是同一个节点**(`AlterRoleStmt` 没有
/// "哪一种"的字段),所以报文里统一写 `ALTER ROLE`--语句原文照旧打在 `[N]` 那一段里.
/// `RENAME TO` 是 `action == -1` 加一条 `newname` 选项.
fn lower_alter_role(role: &AlterRoleStmt) -> Result<Stmt> {
    let name = role_spec(role.role.as_ref())?;
    let mut request = AlterRoleRequest {
        what: "ALTER ROLE",
        name,
        new_name: None,
        password: PasswordAction::Unchanged,
        changes: Vec::new(),
    };
    for option in &role.options {
        let elem = match &option.node {
            Some(node::Node::DefElem(elem)) => elem,
            _ => return Err(unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  "ALTER ROLE")))),
        };

        match elem.defname.as_str() {
            "password" => {
                request.password = match def_string(elem) {
                    Some(text) => {
                        if text.is_empty() {
                            return Err(unsupported(format!("{}", tr!("ir.irgen.197"))));
                        }
                        PasswordAction::Set(Password::new(text))
                    }
                    // `PASSWORD NULL`:清掉
                    None => PasswordAction::Clear,
                };
            }
            "superuser" | "createdb" | "createrole" | "inherit" | "canlogin" => {
                let value = def_bool(elem).ok_or_else(|| {
                    unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  elem.defname)))
                })?;
                request.changes.push(match elem.defname.as_str() {
                    "superuser" => RoleAttrChange::Admin(value),
                    "createdb" => RoleAttrChange::CreateDb(value),
                    "createrole" => RoleAttrChange::CreateRole(value),
                    "inherit" => RoleAttrChange::Inherit(value),
                    _ => RoleAttrChange::Login(value),
                });
            }
            other => {
                return Err(unsupported(format!("{}", tr!("ir.irgen.198", arg0 =  other))))
            }
        }
    }

    Ok(Stmt::AlterRole(request))
}

/// `DROP USER` / `DROP ROLE`(语法树里是同一个节点,报文里统一写 `DROP ROLE`).
fn lower_drop_role(drop: &DropRoleStmt) -> Result<Stmt> {
    let mut names = Vec::new();
    for node in &drop.roles {
        names.push(role_spec(node_role_spec(node))?);
    }
    if names.is_empty() {
        return Err(unsupported("DROP ROLE (no name)"));
    }

    Ok(Stmt::DropRole {
        what: "DROP ROLE",
        names,
        if_exists: drop.missing_ok,
    })
}

/// `GRANT <角色> TO <成员>` / `REVOKE <角色> FROM <成员>`.
///
/// 本版没有 `ADMIN OPTION` 那一档(能不能把角色再授出去由 `CREATEROLE` 决定),
/// 所以 `WITH ADMIN OPTION` 与 `ADMIN OPTION FOR` 都**显式拒绝**:答应下来却不当
/// 回事,等于给了一个假的权限开关.
fn lower_grant_role(grant: &GrantRoleStmt) -> Result<Stmt> {
    if !grant.opt.is_empty() {
        return Err(unsupported("GRANT role ... WITH ADMIN OPTION / ADMIN OPTION FOR"));
    }
    if grant.grantor.is_some() {
        return Err(unsupported("GRANT role ... GRANTED BY"));
    }
    if DropBehavior::try_from(grant.behavior).unwrap_or(DropBehavior::Undefined)
        == DropBehavior::DropCascade
    {
        return Err(unsupported("REVOKE ... CASCADE"));
    }

    let mut roles = Vec::new();
    for node in &grant.granted_roles {
        roles.push(granted_role_name(node)?);
    }
    let mut members = Vec::new();
    for node in &grant.grantee_roles {
        members.push(role_spec(node_role_spec(node))?);
    }
    if roles.is_empty() || members.is_empty() {
        return Err(unsupported(tr!("ir.irgen.202").into_owned()));
    }

    Ok(Stmt::GrantRole(GrantRoleRequest {
        roles,
        members,
        is_grant: grant.is_grant,
    }))
}

/// `GRANT <角色>` 里那半个名字.
///
/// **实测坑**:`GrantRoleStmt.granted_roles` 里的角色名有时不是 `RoleSpec`,而是
/// 一个 `AccessPriv`(它的 `priv_name` 就是角色名,`cols` 为空)--所以两种节点都得认.
fn granted_role_name(node: &Node) -> Result<String> {
    match &node.node {
        Some(node::Node::RoleSpec(spec)) => role_spec(Some(spec)),
        Some(node::Node::AccessPriv(priv_)) if priv_.cols.is_empty() && !priv_.priv_name.is_empty() => {
            Ok(priv_.priv_name.clone())
        }
        _ => Err(unsupported(format!("{}", tr!("ir.irgen.202")))),
    }
}

/// `GRANT` / `REVOKE <对象权限>`:表(可选列清单)或者库上的 `CONNECT`.
///
/// 只认这三样东西:表级 / 列级的 `SELECT` / `INSERT` / `UPDATE`,以及库级的
/// `CONNECT`.别的(`DELETE`,`TRUNCATE`,`USAGE`,`ON FUNCTION`...)**显式拒绝**
/// --本版没有那些能力,授出去就是一张空头支票.
///
/// 两条与 PostgreSQL 的差别(如实写在这儿):
///
/// * `ALL` 在表上展开成**三种**权限(不是 PostgreSQL 的七种);
/// * `ALL` 在库上展开成 `CONNECT`(不是 `CREATE`/`CONNECT`/`TEMPORARY`).
///
/// 两处都在状态行里如实列出到底授了什么,用户一眼看得见少了哪几样.
fn lower_grant(grant: &GrantStmt, symbols: &SymbolTable) -> Result<Stmt> {
    let targtype = GrantTargetType::try_from(grant.targtype).unwrap_or(GrantTargetType::Undefined);
    if targtype != GrantTargetType::AclTargetObject {
        return Err(unsupported(tr!("ir.irgen.206").into_owned()));
    }
    if grant.grant_option {
        return Err(unsupported(tr!("ir.irgen.200", arg0 = "WITH GRANT OPTION")));
    }
    if grant.grantor.is_some() {
        return Err(unsupported(tr!("ir.irgen.200", arg0 = "GRANTED BY")));
    }
    if DropBehavior::try_from(grant.behavior).unwrap_or(DropBehavior::Undefined)
        == DropBehavior::DropCascade
    {
        return Err(unsupported(tr!("ir.irgen.200", arg0 = "CASCADE")));
    }

    let mut grantees = Vec::new();
    for node in &grant.grantees {
        grantees.push(role_spec(node_role_spec(node))?);
    }
    if grantees.is_empty() {
        return Err(unsupported(tr!("ir.irgen.202").into_owned()));
    }

    let objtype = ObjectType::try_from(grant.objtype).unwrap_or(ObjectType::Undefined);
    let (target, wants) = match objtype {
        // 库上只有 `CONNECT` 一件事可授(它是"进得来"这个权限,不是表权限)
        ObjectType::ObjectDatabase => {
            let name = database_name(grant)?;
            for node in &grant.privileges {
                let named = match &node.node {
                    Some(node::Node::AccessPriv(priv_)) => priv_,
                    _ => return Err(unsupported(tr!("ir.irgen.210").into_owned())),
                };
                if !named.cols.is_empty() {
                    return Err(unsupported(tr!("ir.irgen.209").into_owned()));
                }
                // 空 = `ALL`(库上只有 CONNECT,所以也算认了)
                if !named.priv_name.is_empty() && !named.priv_name.eq_ignore_ascii_case("connect") {
                    return Err(unsupported(format!("{}", tr!("ir.irgen.209"))));
                }
            }
            (GrantTarget::Database(name), Vec::new())
        }

        // 表 / 列
        ObjectType::ObjectTable => {
            let mut tables = Vec::new();
            for node in &grant.objects {
                let range = match &node.node {
                    Some(node::Node::RangeVar(range)) => range,
                    _ => return Err(unsupported(tr!("ir.irgen.208").into_owned())),
                };
                let name = qualified_name(range)?;
                // 表必须已经存在:授权对象是**这张表**,不是"以后可能叫这个名字的表"
                let effective = symbols
                    .relation(&name)
                    .ok_or_else(|| DbError::UnknownSymbol { name: name.clone() })?
                    .name()
                    .to_string();
                tables.push((effective, name));
            }
            if tables.is_empty() {
                return Err(unsupported(tr!("ir.irgen.208").into_owned()));
            }

            let wants = lower_grants(grant)?;
            for (_, written) in &tables {
                let all = symbols
                    .columns(written)
                    .ok_or_else(|| DbError::UnknownSymbol {
                        name: written.clone(),
                    })?;
                for want in &wants {
                    for column in &want.columns {
                        if !all.iter().any(|held| &held.name == column) {
                            return Err(DbError::UnknownColumn {
                                name: column.clone(),
                                available: all.iter().map(|held| held.name.clone()).collect(),
                            });
                        }
                    }
                }
            }

            (
                GrantTarget::Tables(tables.into_iter().map(|(effective, _)| effective).collect()),
                wants,
            )
        }

        other => {
            return Err(unsupported(format!("{}", tr!("ir.irgen.199", arg0 = 
                other.as_str_name()
            ))))
        }
    };

    Ok(Stmt::Grant(GrantRequest {
        is_grant: grant.is_grant,
        target,
        grantees,
        grants: wants,
    }))
}

/// `GRANT ... ON DATABASE <名字>` 里那个名字(本版只认一个).
fn database_name(grant: &GrantStmt) -> Result<String> {
    let mut names = Vec::new();
    for node in &grant.objects {
        match const_string(node) {
            Some(name) => names.push(name),
            None => return Err(unsupported(tr!("ir.irgen.208").into_owned())),
        }
    }
    match names.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(unsupported(tr!("ir.irgen.208").into_owned())),
    }
}

/// 表上那几种权限连同各自的列清单(`ALL` 展开成三种).
fn lower_grants(grant: &GrantStmt) -> Result<Vec<PrivilegeGrant>> {
    let mut wants: Vec<PrivilegeGrant> = Vec::new();
    for node in &grant.privileges {
        let named = match &node.node {
            Some(node::Node::AccessPriv(priv_)) => priv_,
            _ => return Err(unsupported(tr!("ir.irgen.210").into_owned())),
        };

        // 列清单:这一条权限只授这几列(`GRANT SELECT (title) ON movies`)
        let mut columns = Vec::new();
        for column in &named.cols {
            match const_string(column) {
                Some(name) => columns.push(name),
                None => return Err(unsupported(tr!("ir.irgen.207").into_owned())),
            }
        }

        // `priv_name` 为空 = `GRANT ALL ...`:本版只有这三种
        let kinds: Vec<Privilege> = if named.priv_name.is_empty() {
            Privilege::all().to_vec()
        } else {
            let name = named.priv_name.to_ascii_uppercase();
            vec![match name.as_str() {
                "SELECT" => Privilege::Select,
                "INSERT" => Privilege::Insert,
                "UPDATE" => Privilege::Update,
                other => {
                    return Err(unsupported(format!("{}", tr!("ir.irgen.203", arg0 = other))))
                }
            }]
        };

        for privilege in kinds {
            match wants.iter_mut().find(|held| held.privilege == privilege) {
                // 同一种权限写了两遍(或者 `ALL` 与具体的混写):列清单并起来
                Some(held) => {
                    for column in &columns {
                        if !held.columns.contains(column) {
                            held.columns.push(column.clone());
                        }
                    }
                }
                None => wants.push(PrivilegeGrant {
                    privilege,
                    columns: columns.clone(),
                }),
            }
        }
    }

    if wants.is_empty() {
        // 权限列表空着 = `ALL`
        wants.extend(Privilege::all().into_iter().map(|privilege| PrivilegeGrant {
            privilege,
            columns: Vec::new(),
        }));
    }
    for want in &mut wants {
        want.columns.sort();
        want.columns.dedup();
    }
    wants.sort_by_key(|want| want.privilege);
    Ok(wants)
}

/// `DROP OWNED BY <角色>`:把这个角色在**当前库**里的授权收回.
///
/// 本版只做当前库(表在同名的库里,别的库的表本会话根本看不到);要清别的库,
/// 先 `\c` 过去.`CASCADE` / `RESTRICT` 一律拒绝(本版没有依赖可级联).
fn lower_drop_owned(drop: &DropOwnedStmt) -> Result<Stmt> {
    if DropBehavior::try_from(drop.behavior).unwrap_or(DropBehavior::Undefined)
        == DropBehavior::DropCascade
    {
        return Err(unsupported(tr!("ir.irgen.200", arg0 = "CASCADE")));
    }
    let mut roles = Vec::new();
    for node in &drop.roles {
        roles.push(role_spec(node_role_spec(node))?);
    }
    if roles.is_empty() {
        return Err(unsupported(tr!("ir.irgen.208").into_owned()));
    }
    Ok(Stmt::DropOwned { roles })
}

/// `ALTER DATABASE <库> OWNER TO <角色>`:换库的 owner.
///
/// 本版**只认这一条** `ALTER DATABASE` 的 owner 写法(`SET search_path` 在
/// `irgen` 的另一处);`RENAME` 照样拒绝.
fn lower_alter_owner(owner: &AlterOwnerStmt) -> Result<Stmt> {
    let objtype = ObjectType::try_from(owner.object_type).unwrap_or(ObjectType::Undefined);
    if objtype != ObjectType::ObjectDatabase {
        return Err(unsupported(tr!("ir.irgen.199", arg0 = objtype.as_str_name())));
    }

    let name = match owner.object.as_deref().and_then(|node| node.node.as_ref()) {
        Some(node::Node::String(text)) => text.sval.clone(),
        _ => return Err(unsupported(tr!("ir.irgen.208").into_owned())),
    };
    let new_owner = role_spec(owner.newowner.as_ref())?;

    Ok(Stmt::SetDatabaseOwner {
        name,
        owner: new_owner,
    })
}

/// `ALTER ROLE r RENAME TO s`:语法树里是 [`RenameStmt`],别的 `RENAME` 照旧拒绝.
///
/// (实测:它不是 `AlterRoleStmt` 加一个负数 `action`--那个写法在本版的解析器上
/// 根本不会出现,别再猜一处.)
fn lower_rename(rename: &RenameStmt) -> Result<Stmt> {
    let kind = ObjectType::try_from(rename.rename_type).unwrap_or(ObjectType::Undefined);
    if kind != ObjectType::ObjectRole {
        return Err(unsupported(format!("{}", tr!("ir.irgen.006"))));
    }
    if rename.subname.is_empty() || rename.newname.is_empty() {
        return Err(unsupported("ALTER ROLE ... RENAME TO"));
    }

    Ok(Stmt::AlterRole(AlterRoleRequest {
        what: "ALTER ROLE",
        name: rename.subname.clone(),
        new_name: Some(rename.newname.clone()),
        password: PasswordAction::Unchanged,
        changes: Vec::new(),
    }))
}

/// `Node` 里那个 `RoleSpec`(没有就报"授权对象必须是用户名字").
fn node_role_spec(node: &Node) -> Option<&RoleSpec> {
    match &node.node {
        Some(node::Node::RoleSpec(spec)) => Some(spec),
        _ => None,
    }
}

/// `RoleSpec` → 角色名.
///
/// 只认写出来的名字:`CURRENT_USER` / `SESSION_USER` / `PUBLIC` 这些**显式拒绝**
/// (本版没有"当前用户"这种写法,也没有 `PUBLIC` 那一档).
fn role_spec(spec: Option<&RoleSpec>) -> Result<String> {
    let spec = spec.ok_or_else(|| unsupported(tr!("ir.irgen.202").into_owned()))?;
    let kind = RoleSpecType::try_from(spec.roletype).unwrap_or(RoleSpecType::Undefined);
    if kind != RoleSpecType::RolespecCstring {
        return Err(unsupported(format!("{}", tr!("ir.irgen.202"))));
    }
    if spec.rolename.trim().is_empty() {
        return Err(unsupported(format!("{}", tr!("ir.irgen.201"))));
    }
    Ok(spec.rolename.clone())
}

/// 一条 `DefElem` 的字符串取值(`PASSWORD 'x'`;空值节点给 `None`).
///
/// `PASSWORD NULL` 是个空值节点(`A_Const` 的 `isnull`),所以 `None` 有两种来源:
/// "不是字符串"与"就是 NULL"--调用方按各自的口径处理(建角色时两者同义,改口令时
/// NULL 是"清掉").
fn def_string(elem: &DefElem) -> Option<String> {
    let arg = elem.arg.as_deref()?;
    match &arg.node {
        Some(node::Node::String(s)) => Some(s.sval.clone()),
        Some(node::Node::AConst(_)) => const_string(arg),
        _ => None,
    }
}

/// 一条 `DefElem` 的布尔取值(`SUPERUSER` / `NOSUPERUSER` 之类).
fn def_bool(elem: &DefElem) -> Option<bool> {
    match elem.arg.as_deref().and_then(|arg| arg.node.as_ref()) {
        Some(node::Node::Boolean(value)) => Some(value.boolval),
        Some(node::Node::Integer(value)) => Some(value.ival != 0),
        _ => None,
    }
}

/// 一条 `DefElem` 里的角色清单(`IN ROLE a, b`).
///
/// 认不出来的写法**拒绝**,不悄悄少加一个成员("少加一个"是那种事后才发现的
/// 安静错误).
fn def_roles(elem: &DefElem) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(arg) = elem.arg.as_deref() {
        collect_role_specs(arg, &mut out)?;
    }
    Ok(out)
}

/// 把 `RoleSpec`(可能是 `List`)收进 `out`,认不出来就报错.
fn collect_role_specs(node: &Node, out: &mut Vec<String>) -> Result<()> {
    match &node.node {
        Some(node::Node::RoleSpec(spec)) => {
            out.push(role_spec(Some(spec))?);
            Ok(())
        }
        Some(node::Node::List(list)) => {
            for item in &list.items {
                collect_role_specs(item, out)?;
            }
            Ok(())
        }
        _ => Err(unsupported(format!("{}", tr!("ir.irgen.202")))),
    }
}

// ------------------------------------------------------------- CREATE INDEX

/// `CREATE [IF NOT EXISTS] INDEX name ON table (col)` 的降级.
///
/// 只认**裸列**,单列,非唯一:本版的索引是"键 → 行号"的哈希表(见
/// [`crate::db::index`]),唯一性它不强制;表达式键与多列键表达不了,于是显式
/// 拒绝--宁可拒绝,也不要给一个安静的错答案.
///
/// 真正建索引的动作在 [`crate::ir::SymbolTable::create_index`] 里(那里才有表).
fn lower_create_index(index: &IndexStmt, symbols: &SymbolTable) -> Result<Stmt> {
    if index.unique || index.primary {
        return Err(unsupported(
            tr!("ir.irgen.035").into_owned(),
        ));
    }
    if index.concurrent {
        return Err(unsupported("CREATE INDEX CONCURRENTLY"));
    }
    if index.where_clause.is_some() {
        return Err(unsupported(tr!("ir.irgen.036").into_owned()));
    }
    if !index.index_including_params.is_empty() {
        return Err(unsupported(tr!("ir.irgen.037").into_owned()));
    }
    if !index.options.is_empty() {
        return Err(unsupported(tr!("ir.irgen.038").into_owned()));
    }
    // 注意:**没写 `USING` 时语法树里也带着默认的 `btree`**(实测),所以这里认
    // `btree` 与空串;别的访问方法一律拒绝--本版的索引只有一种实现,不能假装
    // 它支持某种方法.
    if !index.access_method.is_empty() && index.access_method != "btree" {
        return Err(unsupported(format!("{}", tr!("ir.irgen.039", arg0 = 
            index.access_method
        ))));
    }
    if !index.table_space.is_empty() {
        return Err(unsupported(tr!("ir.irgen.040").into_owned()));
    }
    if index.idxname.is_empty() {
        return Err(unsupported(tr!("ir.irgen.041").into_owned()));
    }
    if index.index_params.len() != 1 {
        return Err(unsupported(format!("{}", tr!("ir.irgen.042", arg0 = 
            index.index_params.len()
        ))));
    }

    let column = match &index.index_params[0].node {
        Some(node::Node::IndexElem(elem)) if !elem.name.is_empty() => elem.name.clone(),
        _ => return Err(unsupported(tr!("ir.irgen.043").into_owned())),
    };

    let relation = index
        .relation
        .as_ref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.044").into_owned()))?;
    let table = qualified_name(relation)?;
    let columns = symbols
        .columns(&table)
        .ok_or_else(|| DbError::UnknownSymbol { name: table.clone() })?;
    let at = columns
        .iter()
        .position(|column_def| column_def.name == column)
        .ok_or_else(|| DbError::UnknownColumn {
            name: column.clone(),
            available: columns.iter().map(|c| c.name.clone()).collect(),
        })?;

    Ok(Stmt::CreateIndex {
        name: index.idxname.clone(),
        table,
        column: at,
        if_not_exists: index.if_not_exists,
    })
}

// ------------------------------------------------------------- CREATE TABLE

/// 降级 `CREATE TABLE`.
///
/// 约束的口径(`filmdb.sql` 的 11 张表全靠这一套):
///
/// * **真强制**:列级 `NOT NULL`(进 [`Column::not_null`]),`DEFAULT`(进
///   [`Column::default`],缺列时补),`CHECK`(降级成 [`Prog`] 挂到表的
///   `CheckedTable` 上,写行时求值);
/// * **接受但如实记录**(进 `unenforced`,建表消息里列出来):`PRIMARY KEY`,
///   `UNIQUE`,`FOREIGN KEY ... REFERENCES ...`--本版不做唯一性与引用检查,
///   也不做 `ON DELETE` / `ON UPDATE` 的动作(没有 `DELETE`,动作没有语义);
/// * **拒绝**(有行为语义或会静默违规的):`DEFERRABLE` / `INITIALLY DEFERRED` /
///   `NOT VALID`,`IDENTITY`,`GENERATED`,`COLLATE`,`EXCLUDE`.
///
/// 表级约束**两种写法都要认**:实测 `CREATE TABLE p (a INT, PRIMARY KEY (a))` 里
/// 那条 `PRIMARY KEY` 会作为 `Constraint` 出现在 `table_elts` 里,而不是
/// `CreateStmt.constraints`--所以循环两处都走.
fn lower_create_table(create: &CreateStmt, symbols: &SymbolTable) -> Result<Stmt> {
    // 本版只认最朴素的 `CREATE TABLE name (列定义, 约束...)`
    if !create.inh_relations.is_empty() {
        return Err(unsupported("INHERITS"));
    }
    if create.partbound.is_some() || create.partspec.is_some() {
        return Err(unsupported("PARTITION BY / PARTITION OF"));
    }
    if create.of_typename.is_some() {
        return Err(unsupported(tr!("ir.irgen.045").into_owned()));
    }
    if !create.access_method.is_empty() {
        return Err(unsupported(tr!("ir.irgen.046").into_owned()));
    }
    if !create.tablespacename.is_empty() {
        return Err(unsupported("TABLESPACE"));
    }
    if !create.options.is_empty() {
        return Err(unsupported(tr!("ir.irgen.047").into_owned()));
    }
    // 注意:`OnCommitAction::OncommitNoop = 1`,**不是** 0.没写 ON COMMIT 时
    // 这个字段就是 1,所以不能拿 `!= 0` 当"没指定".(和 `SetopNone = 1` 同一
    // 类坑:枚举里表示"无"的那个变体不是 0.)
    let on_commit = OnCommitAction::try_from(create.oncommit).unwrap_or(OnCommitAction::Undefined);
    if on_commit != OnCommitAction::OncommitNoop {
        return Err(unsupported("ON COMMIT"));
    }

    let relation = create
        .relation
        .as_ref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.048").into_owned()))?;
    let name = qualified_name(relation)?;
    // 合成的约束名用最后一段(`study.movies` → `movies_..._check`),与 PostgreSQL
    // 的 `ChooseConstraintName` 口径一致
    let short_name = name.rsplit('.').next().unwrap_or(&name).to_string();

    let mut columns: Vec<Column> = Vec::with_capacity(create.table_elts.len());
    let mut unenforced: Vec<String> = Vec::new();
    // `(表达式, 约束名, 合成名)`:**所有列都收齐之后**才降级,因为 CHECK 面对的是
    // 整张表的扁平 schema(列级 CHECK 也一样能引用别的列)
    let mut checks: Vec<(&Node, Option<String>, String)> = Vec::new();

    let mut elts: Vec<&Node> = create.table_elts.iter().collect();
    elts.extend(create.constraints.iter());

    for elt in elts {
        match &elt.node {
            Some(node::Node::ColumnDef(def)) => {
                if def.raw_default.is_some() || def.cooked_default.is_some() {
                    return Err(unsupported(tr!("ir.irgen.049").into_owned()));
                }
                if !def.identity.is_empty() || def.identity_sequence.is_some() {
                    return Err(unsupported(tr!("ir.irgen.050").into_owned()));
                }
                if !def.generated.is_empty() {
                    return Err(unsupported(tr!("ir.irgen.051").into_owned()));
                }
                if def.coll_clause.is_some() {
                    return Err(unsupported("COLLATE"));
                }

                let ty = column_type(
                    def.type_name
                        .as_ref()
                        .ok_or_else(|| unsupported(tr!("ir.irgen.052").into_owned()))?,
                )?;

                let mut not_null = false;
                let mut default: Option<String> = None;
                for constraint in &def.constraints {
                    let c = column_constraint(constraint)?;
                    reject_constraint_attributes(c)?;
                    match ConstrType::try_from(c.contype).unwrap_or(ConstrType::Undefined) {
                        // **真的强制**:`Column::not_null` 会让 `Table::validate` 拦住 NULL.
                        ConstrType::ConstrNotnull => not_null = true,

                        // **真的强制**:缺列时补它(见 `lower_insert`)
                        ConstrType::ConstrDefault => {
                            default = Some(read_default(c, &ty, &def.colname)?);
                        }

                        // **真的强制**:降级成谓词,写行时求值
                        ConstrType::ConstrCheck => {
                            let expr = c
                                .raw_expr
                                .as_deref()
                                .ok_or_else(|| unsupported(tr!("ir.irgen.053").into_owned()))?;
                            let synth = format!("{}_{}_check", short_name, def.colname);
                            checks.push((expr, conname(c), synth));
                        }

                        // 接受,但本版不做唯一性检查,必须让它看得见.
                        ConstrType::ConstrPrimary => {
                            unenforced.push(constraint_text(c, &format!("PRIMARY KEY ({})", def.colname)));
                        }
                        ConstrType::ConstrUnique => {
                            unenforced.push(constraint_text(c, &format!("UNIQUE ({})", def.colname)));
                        }
                        // 外键:接受 + 记录,**不做引用检查**(本版也没有 DELETE,
                        // 所以 ON DELETE 的动作没有语义)
                        ConstrType::ConstrForeign => {
                            unenforced.push(constraint_text(
                                c,
                                &describe_foreign(c, Some(&def.colname))?,
                            ));
                        }

                        ConstrType::ConstrNull => {}
                        other => return Err(unsupported(format!("{}", tr!("ir.irgen.054", arg0 =  other : {:?})))),
                    }
                }

                let mut column = if not_null {
                    Column::not_null(def.colname.clone(), ty)
                } else {
                    Column::new(def.colname.clone(), ty)
                };
                if let Some(text) = default {
                    column = column.with_default(text);
                }
                columns.push(column);
            }

            // 表级约束(`PRIMARY KEY (a, b)` 写在列定义之外)
            Some(node::Node::Constraint(c)) => {
                reject_constraint_attributes(c)?;
                let body = match ConstrType::try_from(c.contype).unwrap_or(ConstrType::Undefined) {
                    ConstrType::ConstrPrimary => {
                        format!("PRIMARY KEY ({})", constraint_keys(&c.keys)?)
                    }
                    ConstrType::ConstrUnique => format!("UNIQUE ({})", constraint_keys(&c.keys)?),
                    ConstrType::ConstrForeign => describe_foreign(c, None)?,
                    ConstrType::ConstrCheck => {
                        let expr = c
                            .raw_expr
                            .as_deref()
                            .ok_or_else(|| unsupported(tr!("ir.irgen.055").into_owned()))?;
                        let synth = format!("{}_check", short_name);
                        checks.push((expr, conname(c), synth));
                        continue;
                    }
                    other => {
                        return Err(unsupported(format!("{}", tr!("ir.irgen.056", arg0 =  other : {:?}))));
                    }
                };
                unenforced.push(constraint_text(c, &body));
            }

            _ => {
                return Err(unsupported(
                    tr!("ir.irgen.057").into_owned(),
                ))
            }
        }
    }

    if columns.is_empty() {
        return Err(unsupported(tr!("ir.irgen.058").into_owned()));
    }

    // CHECK 现在才降级:它面对**整张表**的扁平 schema
    let scope = Scope::from_range(name.clone(), None, columns.clone(), None, symbols);
    let mut lowered_checks = Vec::with_capacity(checks.len());
    for (expr, name, synth) in checks {
        let mut p = ProgBuilder::new();
        let reg = lower_expr(expr, &scope, &mut p)?;
        let program = p.finish(reg);
        if program.has_subquery() || program.has_outer() {
            return Err(unsupported(tr!("ir.irgen.059").into_owned()));
        }
        if program.verify(&columns)? != ScalarType::Bool {
            return Err(unsupported(tr!("ir.irgen.060").into_owned()));
        }
        lowered_checks.push(Check {
            name: name.unwrap_or(synth),
            program,
        });
    }

    Ok(Stmt::CreateTable {
        name,
        columns,
        if_not_exists: create.if_not_exists,
        unenforced,
        checks: lowered_checks,
    })
}

/// 一个列约束项 → 它的 `Constraint`.
fn column_constraint(node: &Node) -> Result<&pg_query::protobuf::Constraint> {
    match &node.node {
        Some(node::Node::Constraint(c)) => Ok(c),
        _ => Err(unsupported(tr!("ir.irgen.061").into_owned())),
    }
}

/// 约束名(没写就是空串).
fn conname(c: &pg_query::protobuf::Constraint) -> Option<String> {
    if c.conname.is_empty() {
        None
    } else {
        Some(c.conname.clone())
    }
}

/// 有行为语义,不能假装的约束属性:一律拒绝.
fn reject_constraint_attributes(c: &pg_query::protobuf::Constraint) -> Result<()> {
    if c.deferrable || c.initdeferred {
        return Err(unsupported(tr!("ir.irgen.062").into_owned()));
    }
    if c.skip_validation {
        return Err(unsupported(tr!("ir.irgen.063").into_owned()));
    }
    Ok(())
}

/// 记录用的约束文本:写了名字就把名字带上.
fn constraint_text(c: &pg_query::protobuf::Constraint, body: &str) -> String {
    match conname(c) {
        Some(name) => format!("CONSTRAINT {} {}", name, body),
        None => body.to_string(),
    }
}

/// `PRIMARY KEY (...)` / `UNIQUE (...)` 的列名表.
fn constraint_keys(keys: &[Node]) -> Result<String> {
    let mut names = Vec::with_capacity(keys.len());
    for key in keys {
        let name = match &key.node {
            Some(node::Node::String(s)) => s.sval.clone(),
            // 实测列名也可能是裸 `String` 之外的列引用
            Some(node::Node::ColumnRef(c)) => ref_parts(&c.fields)
                .map(|parts| parts.join("."))
                .ok_or_else(|| unsupported(tr!("ir.irgen.064").into_owned()))?,
            _ => return Err(unsupported(tr!("ir.irgen.065").into_owned())),
        };
        names.push(name);
    }
    Ok(names.join(", "))
}

/// `FOREIGN KEY (...) REFERENCES t(...) [ON DELETE ...] [ON UPDATE ...]` 的文本.
///
/// 只用来**如实记录**(本版不强制外键),所以认不出的动作也照样写出来.
fn describe_foreign(c: &pg_query::protobuf::Constraint, column: Option<&str>) -> Result<String> {
    let local = match column {
        Some(column) => column.to_string(),
        None => constraint_keys(&c.fk_attrs)?,
    };
    let table = match &c.pktable {
        Some(rv) => qualified_name(rv)?,
        None => return Err(unsupported(tr!("ir.irgen.066").into_owned())),
    };
    let referenced = constraint_keys(&c.pk_attrs)?;

    let mut text = format!("FOREIGN KEY ({}) REFERENCES {}({})", local, table, referenced);

    if !c.fk_matchtype.is_empty() && c.fk_matchtype != "s" {
        text.push_str(&format!(" MATCH {}", fk_match_name(&c.fk_matchtype)));
    }
    if !c.fk_del_action.is_empty() {
        text.push_str(&format!(" ON DELETE {}", fk_action_name(&c.fk_del_action)));
    }
    if !c.fk_upd_action.is_empty() {
        text.push_str(&format!(" ON UPDATE {}", fk_action_name(&c.fk_upd_action)));
    }
    Ok(text)
}

/// 外键动作的单字母编码 → 名字(PostgreSQL 语法树里就是这么编的).
fn fk_action_name(action: &str) -> String {
    match action {
        "a" => "NO ACTION".to_string(),
        "r" => "RESTRICT".to_string(),
        "c" => "CASCADE".to_string(),
        "n" => "SET NULL".to_string(),
        "d" => "SET DEFAULT".to_string(),
        other => other.to_string(),
    }
}

/// 外键 `MATCH` 的单字母编码 → 名字.
fn fk_match_name(matchtype: &str) -> String {
    match matchtype {
        "f" => "FULL".to_string(),
        "p" => "PARTIAL".to_string(),
        other => other.to_string(),
    }
}

/// 降级一个 `DEFAULT` 字面量.
///
/// 只认常量字面量(`DEFAULT now()`,`DEFAULT 1 + 1` 一律拒绝--本版没有"每次插入
/// 重新求值"的机制,猜一个固定值会给出安静的错误答案).值按列类型解析并强制
/// typmod,再存成 **CSV 字段形式**(`NULL` 就是 `\N`).
fn read_default(c: &pg_query::protobuf::Constraint, ty: &DataType, column: &str) -> Result<String> {
    let expr = c
        .raw_expr
        .as_deref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.067").into_owned()))?;
    let konst = match &expr.node {
        Some(node::Node::AConst(konst)) => konst,
        _ => {
            return Err(unsupported(
                tr!("ir.irgen.068").into_owned(),
            ))
        }
    };
    if konst.isnull {
        return Ok(Value::Null.to_csv_field());
    }

    let text = literal_text(konst).ok_or_else(|| unsupported(tr!("ir.irgen.069").into_owned()))?;
    let value = ty.parse(&text).map_err(|e| match at_value(&text, e) {
        DbError::BadValue {
            ty, value, reason, ..
        } => DbError::BadValue {
            column: Some(column.to_string()),
            ty,
            value,
            reason,
        },
        other => other,
    })?;
    Ok(value.to_csv_field())
}

/// `TypeName` → 本版的 [`DataType`].
///
/// 关键:`INT` 在语法树里是 `names = ["pg_catalog", "int4"]`--**内部的,带
/// schema 前缀的**名字.所以取**最后一段**再认.
fn column_type(type_name: &TypeName) -> Result<DataType> {
    if !type_name.array_bounds.is_empty() {
        return Err(unsupported(tr!("ir.irgen.070").into_owned()));
    }
    let last = type_name
        .names
        .last()
        .and_then(|n| match &n.node {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .ok_or_else(|| unsupported(tr!("ir.irgen.071").into_owned()))?;

    // typmods 是**裸字面量**:`VARCHAR(30)` 给 `[Ival(30)]`,`NUMERIC(10,2)` 给
    // `[Ival(10), Ival(2)]`.没有 PostgreSQL 内部那种 N+4 偏移.
    let mods = type_mods(type_name)?;
    let need = |what: &str, index: usize| -> Result<u32> {
        mods.get(index).copied().ok_or_else(|| {
            unsupported(format!("{}", tr!("ir.irgen.072", arg0 = 
                what, arg1 =  what
            )))
        })
    };

    match last.as_str() {
        "int4" | "int" | "integer" => Ok(DataType::Int),
        "text" => Ok(DataType::Text),
        // 语法树里 `CHAR` 的实名是 `bpchar`
        "bpchar" | "char" => Ok(DataType::Char(match mods.first() {
            // SQL 标准:裸 `CHAR` 就是 `CHAR(1)`
            None => 1,
            Some(n) => *n,
        })),
        "varchar" => Ok(DataType::VarChar(need("VARCHAR", 0)?)),
        // `DATE` 没有参数:PostgreSQL 的 `date` 也不带 typmod
        "date" => Ok(DataType::Date),
        "timestamp" | "timestamptz" => Ok(DataType::Timestamp(match mods.first() {
            None => DEFAULT_TIMESTAMP_PRECISION,
            Some(n) if *n <= MAX_TIMESTAMP_PRECISION => *n,
            Some(n) => {
                return Err(unsupported(format!("{}", tr!("ir.irgen.073", arg0 = 
                    n, arg1 =  MAX_TIMESTAMP_PRECISION
                ))))
            }
        })),
        "numeric" | "decimal" => match (mods.first(), mods.get(1)) {
            // `NUMERIC(N, M)`
            (Some(p), Some(s)) => Ok(DataType::Numeric(*p, *s)),
            // `NUMERIC(N)`:与 PostgreSQL 一致,等于标度 0
            (Some(p), None) => Ok(DataType::Numeric(*p, 0)),
            // 裸 `NUMERIC`:精度与标度都不限
            _ => Ok(DataType::NumericAny),
        },
        other => Err(unsupported(format!("{}", tr!("ir.irgen.074", arg0 =  other)))),
    }
}

/// 取出 `TypeName.typmods` 里的整数字面量.
fn type_mods(type_name: &TypeName) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(type_name.typmods.len());
    for m in &type_name.typmods {
        match &m.node {
            Some(node::Node::AConst(c)) => match &c.val {
                Some(Val::Ival(i)) if i.ival >= 0 => out.push(i.ival as u32),
                _ => return Err(unsupported(tr!("ir.irgen.075").into_owned())),
            },
            _ => return Err(unsupported(tr!("ir.irgen.076").into_owned())),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------- INSERT

fn lower_insert(insert: &InsertStmt, symbols: &SymbolTable) -> Result<Stmt> {
    if insert.on_conflict_clause.is_some() {
        return Err(unsupported("ON CONFLICT"));
    }
    if !insert.returning_list.is_empty() {
        return Err(unsupported("RETURNING"));
    }
    if insert.with_clause.is_some() {
        return Err(unsupported(tr!("ir.irgen.077").into_owned()));
    }

    let relation = insert
        .relation
        .as_ref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.078").into_owned()))?;
    let table = qualified_name(relation)?;

    let target = symbols
        .columns(&table)
        .ok_or_else(|| DbError::UnknownSymbol { name: table.clone() })?
        .to_vec();

    // 行数据在 select_stmt 里:`VALUES (...), (...)` 会变成带 values_lists 的
    // SelectStmt,而 `INSERT ... SELECT` 的 values_lists 是空的.
    let select = match insert.select_stmt.as_deref().and_then(|s| s.node.as_ref()) {
        Some(node::Node::SelectStmt(select)) => select,
        _ => return Err(unsupported(tr!("ir.irgen.079").into_owned())),
    };
    if select.values_lists.is_empty() {
        if select.target_list.is_empty() {
            return Err(unsupported("INSERT ... DEFAULT VALUES"));
        }
        return Err(unsupported("INSERT ... SELECT"));
    }

    // 列名表 → "表的第 i 列的值在行内第几个位置";`None` = 这一列没给值,
    // 要补 `DEFAULT`(没有默认值就补 NULL,与 PostgreSQL 一致)
    let permutation = column_permutation(insert, &target)?;

    // 这条语句**写了**哪几列(权限检查用):没写列名时是全部列;`DEFAULT` 补出来的
    // 列不算用户写的(与 PostgreSQL 一致)
    let written: Vec<usize> = permutation
        .iter()
        .enumerate()
        .filter_map(|(index, source)| source.map(|_| index))
        .collect();

    let mut rows = Vec::with_capacity(select.values_lists.len());
    for values in &select.values_lists {
        let items = match &values.node {
            Some(node::Node::List(list)) => &list.items,
            _ => return Err(unsupported(tr!("ir.irgen.080").into_owned())),
        };
        // 写全列时值个数必须与列数相同;写了列名表时与列名表的长度相同
        let wanted = if insert.cols.is_empty() {
            target.len()
        } else {
            insert.cols.len()
        };
        if items.len() != wanted {
            return Err(unsupported(format!("{}", tr!("ir.irgen.081", arg0 = 
                wanted, arg1 = 
                items.len()
            ))));
        }

        // 按表列序重排.值表达式没有输入行,所以作用域是空的--
        // 一旦里面出现列引用,Prog::verify 就会以"列下标越界"把它挡下来.
        let scope = Scope::empty(symbols);
        let mut row = Vec::with_capacity(target.len());
        for (index, source) in permutation.iter().enumerate() {
            let column = &target[index];
            let mut p = ProgBuilder::new();
            let reg = match source {
                // 用目标列的类型给这个值定型(`'Alice'` → TEXT,`'2024-01-15'` →
                // TIMESTAMP,`1.5` → NUMERIC).不做这一步,新类型基本插不进数据.
                Some(source) => lower_expr_typed(&items[*source], &scope, Some(&column.ty), &mut p)?,
                // 省略的列:有 `DEFAULT` 就补它,否则补 NULL
                None => {
                    let value = default_value(column)?;
                    p.value(value)
                }
            };
            row.push(p.finish(reg));
        }
        // 值程序是不建计划直接 `eval` 的:子查询在这里没有求值环境,显式拒绝
        for program in &row {
            reject_subqueries(program, &tr!("ir.irgen.082").into_owned())?;
        }
        rows.push(row);
    }

    Ok(Stmt::Insert {
        table,
        columns: written,
        rows,
    })
}

/// 一条省略列的补值:`DEFAULT`(存的是 CSV 字段形式)或 NULL.
fn default_value(column: &Column) -> Result<Value> {
    match column.default_text() {
        // CSV 字段形式 → 值:`\N` 是 NULL,以 `\` 开头的文本要反转义
        Some(text) => column.ty.from_csv_field(text).map_err(|e| match e {
            DbError::BadValue {
                ty, value, reason, ..
            } => DbError::BadValue {
                column: Some(column.name.clone()),
                ty,
                value,
                reason: format!("{}", tr!("ir.irgen.083", arg0 =  reason)),
            },
            other => other,
        }),
        None => Ok(Value::Null),
    }
}

/// 算出"表的第 i 列,值应该从行内第几个位置取";`None` 表示这一列被省略了.
fn column_permutation(insert: &InsertStmt, target: &[Column]) -> Result<Vec<Option<usize>>> {
    // 没写列名表:值就按表的列序
    if insert.cols.is_empty() {
        return Ok((0..target.len()).map(Some).collect());
    }

    let mut wanted: BTreeMap<String, usize> = BTreeMap::new();
    for (position, col) in insert.cols.iter().enumerate() {
        let name = match &col.node {
            Some(node::Node::ResTarget(rt)) => rt.name.clone(),
            _ => return Err(unsupported(tr!("ir.irgen.084").into_owned())),
        };
        if wanted.insert(name.clone(), position).is_some() {
            return Err(unsupported(format!("{}", tr!("ir.irgen.085", arg0 =  name))));
        }
    }

    // **先**查"表上根本没有这一列".顺序很重要:若先按表列找,用户把列名写错时
    // 会先撞上"某一列没被覆盖",于是收到"你省略了那一列"--完全误导.实测就是
    // 这样,被测试逮住了.
    let table_names: BTreeSet<&str> = target.iter().map(|c| c.name.as_str()).collect();
    let unknown: Vec<String> = wanted
        .keys()
        .filter(|name| !table_names.contains(name.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(DbError::UnknownColumn {
            name: unknown.join(", "),
            available: target.iter().map(|c| c.name.clone()).collect(),
        });
    }

    // 到这里 wanted 里的名字都是真列名;没被点到的列就是**省略**的列
    // (补 `DEFAULT`,没有默认值就补 NULL--PostgreSQL 就是这么定的).
    let mut permutation = Vec::with_capacity(target.len());
    for column in target {
        permutation.push(wanted.get(&column.name).copied());
    }

    Ok(permutation)
}

// ------------------------------------------------------------------- UPDATE

/// 降级 `UPDATE ... SET ... [WHERE ...]`.
///
/// 值与 `WHERE` 都是**按整张表**的扁平 schema 编的 `Prog`,两者都在**旧行**上求值
/// (与 PostgreSQL 一致:`SET a = b, b = a` 换的是两列原本的值).执行是"顺序扫一
/// 遍,逐行改"(见 [`Stmt::Update`])--没有 JOIN,没有 `RETURNING`.
///
/// 这里显式拒绝的写法:`WITH`,`UPDATE ... FROM`,`RETURNING`,下标/字段赋值,
/// 同一列赋值两次,以及值与 `WHERE` 里的**子查询**(那里没有求值环境:整条语句
/// 不建计划,程序直接在整行上 `eval`).
fn lower_update(update: &UpdateStmt, symbols: &SymbolTable) -> Result<Stmt> {
    if update.with_clause.is_some() {
        return Err(unsupported(tr!("ir.irgen.086").into_owned()));
    }
    if !update.from_clause.is_empty() {
        return Err(unsupported(tr!("ir.irgen.087").into_owned()));
    }
    if !update.returning_list.is_empty() {
        return Err(unsupported("RETURNING"));
    }

    let relation = update
        .relation
        .as_ref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.088").into_owned()))?;
    let table = qualified_name(relation)?;
    let alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.clone())
        .filter(|s| !s.is_empty());
    if let Some(a) = &relation.alias {
        if !a.colnames.is_empty() {
            return Err(unsupported(tr!("ir.irgen.089").into_owned()));
        }
    }

    let columns = symbols
        .columns(&table)
        .ok_or_else(|| DbError::UnknownSymbol { name: table.clone() })?
        .to_vec();
    // `scan: None`:`UPDATE` 不建计划,所以这一项既不下推也不走索引
    let scope = Scope::from_range(table.clone(), alias, columns.clone(), None, symbols);

    let mut sets: Vec<(usize, Prog)> = Vec::with_capacity(update.target_list.len());
    for target in &update.target_list {
        let rt = match &target.node {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => return Err(unsupported(tr!("ir.irgen.090").into_owned())),
        };
        if !rt.indirection.is_empty() {
            return Err(unsupported(tr!("ir.irgen.091").into_owned()));
        }
        if rt.name.is_empty() {
            return Err(unsupported(tr!("ir.irgen.092").into_owned()));
        }

        let index = columns
            .iter()
            .position(|column| column.name == rt.name)
            .ok_or_else(|| DbError::UnknownColumn {
                name: rt.name.clone(),
                available: columns.iter().map(|c| c.name.clone()).collect(),
            })?;
        if sets.iter().any(|(at, _)| *at == index) {
            return Err(unsupported(format!("{}", tr!("ir.irgen.093", arg0 =  rt.name))));
        }

        let value = rt.val.as_deref().ok_or_else(|| unsupported(tr!("ir.irgen.094").into_owned()))?;
        let mut p = ProgBuilder::new();
        // 用目标列的类型给字面量定型(与 INSERT 一样)
        let reg = lower_expr_typed(value, &scope, Some(&columns[index].ty), &mut p)?;
        let program = p.finish(reg);
        reject_subqueries(&program, &tr!("ir.irgen.095").into_owned())?;
        if program.verify(&columns)? == ScalarType::Bool {
            return Err(unsupported(format!("{}", tr!("ir.irgen.096", arg0 = 
                rt.name
            ))));
        }
        sets.push((index, program));
    }

    if sets.is_empty() {
        return Err(unsupported(tr!("ir.irgen.097").into_owned()));
    }

    let filter = match &update.where_clause {
        Some(expr) => {
            let mut p = ProgBuilder::new();
            let reg = lower_expr(expr, &scope, &mut p)?;
            let program = p.finish(reg);
            reject_subqueries(&program, &tr!("ir.irgen.098").into_owned())?;
            if program.verify(&columns)? != ScalarType::Bool {
                return Err(unsupported(tr!("ir.irgen.099").into_owned()));
            }
            Some(program)
        }
        None => None,
    };

    Ok(Stmt::Update {
        table,
        sets,
        filter,
    })
}

/// 这个程序里有子查询 / 外层引用吗;有就拒绝.
///
/// `INSERT` 的值,`UPDATE` 的值与 `WHERE`,`CHECK` 都是**不建计划**直接 `eval` 的
/// 程序,没有 [`crate::ir::exec`] 的求值环境--宁可在这里显式拒绝,也不要等执行
/// 时报"需要求值环境".
fn reject_subqueries(program: &Prog, what: &str) -> Result<()> {
    if program.has_subquery() || program.has_outer() {
        return Err(unsupported(format!("{}", tr!("ir.irgen.100", arg0 =  what))));
    }
    Ok(())
}

// ------------------------------------------------------------------ SELECT

fn lower_select(select: &SelectStmt, symbols: &SymbolTable, outers: &[Scope<'_>]) -> Result<Plan> {
    // ---- 守卫:本版表达不了的 SELECT 形态,一律显式拒绝
    let set_op = SetOperation::try_from(select.op).unwrap_or(SetOperation::Undefined);
    if set_op != SetOperation::SetopNone {
        // `SELECT ... UNION SELECT ...`:两支各自降级,再用 `Op::Union` 接起来
        return lower_setop(select, set_op, symbols, outers);
    }
    if select.with_clause.is_some() {
        return Err(unsupported(tr!("ir.irgen.101").into_owned()));
    }
    let distinct = select_distinct(select)?;
    if !select.window_clause.is_empty() {
        return Err(unsupported(tr!("ir.irgen.102").into_owned()));
    }
    if !select.values_lists.is_empty() {
        return Err(unsupported(tr!("ir.irgen.103").into_owned()));
    }
    if select.into_clause.is_some() {
        return Err(unsupported("SELECT INTO"));
    }
    if !select.locking_clause.is_empty() {
        return Err(unsupported("FOR UPDATE / FOR SHARE"));
    }
    if select.limit_option == LimitOption::WithTies as i32 {
        return Err(unsupported("FETCH ... WITH TIES"));
    }
    if select.target_list.is_empty() {
        return Err(DbError::IrInvalid {
            node: None,
            message: tr!("ir.irgen.104").into_owned(),
        });
    }

    let mut b = PlanBuilder::new();

    // ---- FROM
    //
    // 外层链在这一步就挂上去:`WHERE` / 输出列表 / `ORDER BY` 里的相关子查询都要
    // 沿着它往外解名字.
    let (from_id, scope) = lower_from(&select.from_clause, &mut b, symbols, outers)?;
    let schema = scope.schema();

    // ---- WHERE(Filter 不改变 schema,所以后面几个都还按同一个 scope 解)
    //
    // 能推到单表的合取项就推(见 `opt::pushdown`),推不动的留在 `Filter` 里.
    let filtered = match &select.where_clause {
        Some(expr) => opt::pushdown::plan_where(expr, from_id, &scope, &mut b)?,
        None => from_id,
    };

    // ---- 是分组查询吗
    //
    // 三种情况都算:写了 GROUP BY;写了 HAVING;输出列表或 ORDER BY 里出现了聚合.
    // 最后一种正是 `SELECT COUNT(*) FROM t`(整表一组).
    let grouped = !select.group_clause.is_empty()
        || select.having_clause.is_some()
        || select.target_list.iter().any(contains_aggregate)
        || select.sort_clause.iter().any(contains_aggregate);

    if grouped {
        return lower_grouped(select, filtered, &scope, &schema, b);
    }

    // ---- 输出列
    let (progs, names) = lower_target_list(&select.target_list, &scope)?;
    let identity = is_identity(&progs, &names, &schema);

    // ---- ORDER BY
    //
    // SQL 里 `ORDER BY` 既能用输出列的别名,输出列序号,也能用输入列,两者解析规则
    // 不同.做法是**两遍**:
    //
    // 1. 先按**输出 schema** 解(序号在这里才认),解得动就把 Sort 放在 Project 之上;
    // 2. 解不动再按**输入 schema** 解:这一遍会把输出别名与序号换成目标项自己的
    //    表达式(`p.surname` 这种限定列名只能在输入作用域解),Sort 放在 Project 之下.
    //
    // 但**只要出现了输出列序号,就不许走第 2 遍**:序号退回输入作用域会被当成
    // 常量,排出来的是"常量序"--那正是 TODO 0.1 要修掉的那个假通过.
    let output_columns: Vec<Column> = progs
        .iter()
        .zip(names.iter())
        .map(|(prog, name)| project_column(prog, name, &schema))
        .collect::<Result<_>>()?;
    let output_scope =
        Scope::from_range(String::new(), None, output_columns, None, symbols);

    let (sort_keys, sort_below_project) = if select.sort_clause.is_empty() {
        (Vec::new(), false)
    } else {
        match lower_sort_keys(&select.sort_clause, &output_scope, true) {
            Ok(keys) => (keys, false),
            Err(above) => {
                if sort_clause_has_ordinal(&select.sort_clause) {
                    return Err(above);
                }
                match lower_sort_keys_below(&select.sort_clause, &select.target_list, &scope) {
                    Ok(keys) => (keys, true),
                    Err(_) => return Err(above),
                }
            }
        }
    };

    // ---- 拼装:Sort(下) → Project → Sort(上) → Limit
    let mut current = filtered;

    // 没有 ORDER BY 时 sort_keys 为空,**不生成 Sort 节点**
    // (空排序键是非法计划,analyze 会拒绝).
    if sort_below_project && !sort_keys.is_empty() {
        current = b.sort(current, sort_keys.clone())?;
    }

    if !identity {
        current = b.project(current, progs, names.clone())?;
    }

    // `SELECT DISTINCT`:去的是**输出行**,所以 Distinct 挂在 Project 之上
    if distinct {
        current = b.distinct(current)?;
    }

    if !sort_below_project && !sort_keys.is_empty() {
        current = b.sort(current, sort_keys.clone())?;
    }

    let (limit, offset) = lower_limit(select)?;
    if limit.is_some() || offset > 0 {
        current = b.limit(current, limit, offset)?;
    }

    b.finish(current)
}

/// 分组查询的管道:
///
/// ```text
/// Limit( Sort( Project( Filter(HAVING)( Aggregate( 输入 ) ) ) ) )
/// ```
///
/// 顺序上有一处**不能动**:`SELECT` 列表与 `HAVING` 都要在**建 Aggregate 节点之前**
/// 降级完,因为 `HAVING` 里可能第一次出现某个聚合,后建节点才能把它的规格收全.
fn lower_grouped(
    select: &SelectStmt,
    input: NodeId,
    scope: &Scope<'_>,
    input_schema: &[Column],
    mut b: PlanBuilder,
) -> Result<Plan> {
    let mut agg = AggLowering::default();

    // ---- GROUP BY 的分组键(按输入行求值)
    for group in &select.group_clause {
        if matches!(group.node, Some(node::Node::GroupingSet(_))) {
            return Err(unsupported("GROUPING SETS / ROLLUP / CUBE"));
        }
        let mut p = ProgBuilder::new();
        let reg = lower_expr(group, scope, &mut p)?;
        agg.groups.push(p.finish(reg));
        agg.group_names.push(derived_name(group));
    }

    // ---- SELECT 列表与 HAVING(外层),都先降级,把聚合收齐
    let (progs, names) = lower_target_list_agg(&select.target_list, scope, &mut agg)?;

    let having = match &select.having_clause {
        Some(expr) => {
            let mut p = ProgBuilder::new();
            let reg = lower_expr_agg(expr, scope, &mut agg, &mut p)?;
            Some(p.finish(reg))
        }
        None => None,
    };

    // ---- ORDER BY 的排序键(也要在**建 Aggregate 之前**降级:排序键里可能
    //      第一次出现某个聚合,例如 `ORDER BY AVG(x)`)
    let mut sort_keys = Vec::with_capacity(select.sort_clause.len());
    for item in &select.sort_clause {
        let sb = sort_by(item)?;
        let (desc, nulls_first) = sort_direction(sb)?;

        let expr = sb
            .node
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.105").into_owned()))?;
        // 输出列序号或输出别名都先换成目标项的表达式,于是所有键都按分组降级,
        // 统一索引聚合输出
        let expr = match sort_ordinal_literal(expr) {
            Some(n) => target_expr(&select.target_list, n)?,
            None => substitute_alias(expr, &select.target_list),
        };

        let mut p = ProgBuilder::new();
        let reg = lower_expr_agg(expr, scope, &mut agg, &mut p)?;
        sort_keys.push(SortKey {
            prog: p.finish(reg),
            desc,
            nulls_first,
        });
    }

    // ---- 现在聚合规格齐了,建 Aggregate
    let agg_names: Vec<String> = agg
        .specs
        .iter()
        .map(|spec| spec.func.name().to_lowercase())
        .collect();
    // 这里只为数量对得上做一次校验;`analyze` 才是权威
    let _ = aggregate_schema(
        &agg.groups,
        &agg.group_names,
        &agg.specs,
        &agg_names,
        input_schema,
    )?;

    let mut all_names = agg.group_names.clone();
    all_names.extend(agg_names.iter().cloned());

    let mut current = b.push(
        Op::Aggregate {
            group_by: agg.groups.clone(),
            aggs: agg.specs.clone(),
            names: all_names,
        },
        vec![input],
    )?;

    // ---- HAVING:在 Aggregate 之上,Project 之下(所以用不了输出别名,与 PG 一致)
    if let Some(having) = having {
        current = b.filter(current, having)?;
    }

    // ---- ORDER BY:排序键索引的是**聚合输出**,所以 Sort 必须在 Project 之下
    if !sort_keys.is_empty() {
        current = b.sort(current, sort_keys)?;
    }

    // ---- 投影
    current = b.project(current, progs, names.clone())?;

    // `SELECT DISTINCT`:去的是**输出行**,所以 Distinct 挂在 Project 之上
    if select_distinct(select)? {
        current = b.distinct(current)?;
    }

    let (limit, offset) = lower_limit(select)?;
    if limit.is_some() || offset > 0 {
        current = b.limit(current, limit, offset)?;
    }

    b.finish(current)
}

/// `SELECT DISTINCT` 吗.
///
/// 语法树里裸 `DISTINCT` 是 **`list_make1(NIL)`**--一个"空节点"的列表(实测;
/// 不是 `A_Star`).`DISTINCT ON (...)` 则是实打实的表达式列表,本版的扁平 schema
/// 表达不了"每组留一行",所以明确拒绝.
fn select_distinct(select: &SelectStmt) -> Result<bool> {
    match select.distinct_clause.as_slice() {
        [] => Ok(false),
        [only] => match &only.node {
            None | Some(node::Node::AStar(_)) => Ok(true),
            _ => Err(unsupported("DISTINCT ON (...)")),
        },
        _ => Err(unsupported("DISTINCT ON (...)")),
    }
}

fn lower_limit(select: &SelectStmt) -> Result<(Option<u64>, u64)> {
    let limit = match &select.limit_count {
        Some(node) => Some(const_u64(node, "LIMIT")?),
        None => None,
    };
    let offset = match &select.limit_offset {
        Some(node) => const_u64(node, "OFFSET")?,
        None => 0,
    };
    Ok((limit, offset))
}

fn const_u64(node: &Node, what: &str) -> Result<u64> {
    match &node.node {
        Some(node::Node::AConst(c)) => match &c.val {
            Some(Val::Ival(i)) if i.ival >= 0 => Ok(i.ival as u64),
            Some(Val::Fval(f)) => f
                .fval
                .parse::<u64>()
                .map_err(|_| unsupported(format!("{}", tr!("ir.irgen.106", arg0 =  what)))),
            _ => Err(unsupported(format!("{}", tr!("ir.irgen.107", arg0 =  what)))),
        },
        _ => Err(unsupported(format!("{}", tr!("ir.irgen.108", arg0 =  what)))),
    }
}

// -------------------------------------------------------------------- FROM

fn lower_from<'s>(
    from: &[Node],
    b: &mut PlanBuilder,
    symbols: &'s SymbolTable,
    outers: &[Scope<'s>],
) -> Result<(NodeId, Scope<'s>)> {
    // 没有 FROM:`SELECT 1 + 1` 这种,底座是单行零列.
    if from.is_empty() {
        return Ok((
            b.single_row(),
            Scope::empty(symbols).nested_in(outers.to_vec()),
        ));
    }

    let mut iter = from.iter();
    let first = iter.next().expect(&tr!("ir.irgen.109").into_owned());
    let (mut id, mut scope) = lower_from_item(first, b, symbols, outers)?;

    // `FROM a, b`:pg_query 给的是两个元素,**没有 JoinExpr**.
    // 左深折叠成无条件内连接(笛卡尔积),过滤条件由 WHERE 提供.
    for item in iter {
        let (right_id, right_scope) = lower_from_item(item, b, symbols, outers)?;
        id = b.join(
            id,
            right_id,
            JoinKind::Inner,
            None,
            JoinStrategy::NestedLoop,
        )?;
        scope = Scope::concat(scope, right_scope);
    }

    Ok((id, scope))
}

fn lower_from_item<'s>(
    node: &Node,
    b: &mut PlanBuilder,
    symbols: &'s SymbolTable,
    outers: &[Scope<'s>],
) -> Result<(NodeId, Scope<'s>)> {
    match &node.node {
        Some(node::Node::RangeVar(rv)) => {
            let name = qualified_name(rv)?;
            let columns = symbols
                .columns(&name)
                .ok_or_else(|| DbError::UnknownSymbol { name: name.clone() })?
                .to_vec();

            let alias = rv
                .alias
                .as_ref()
                .map(|a| a.aliasname.clone())
                .filter(|s| !s.is_empty());
            if let Some(a) = &rv.alias {
                if !a.colnames.is_empty() {
                    return Err(unsupported(tr!("ir.irgen.110").into_owned()));
                }
            }

            let id = b.scan(&name);
            // 别名只用于限定名匹配(`t.x`),**不改列名**--列名保持表自己的,
            // 这样 Scope 的 schema 才与 `Plan::analyze` 推出来的一致.
            Ok((
                id,
                Scope::from_range(name, alias, columns, Some(id), symbols)
                    .nested_in(outers.to_vec()),
            ))
        }

        Some(node::Node::JoinExpr(j)) => lower_join(j, b, symbols, outers),

        Some(node::Node::RangeSubselect(sub)) => {
            if sub.lateral {
                return Err(unsupported("LATERAL"));
            }
            let inner = match sub.subquery.as_deref().and_then(|n| n.node.as_ref()) {
                Some(node::Node::SelectStmt(select)) => select,
                _ => return Err(unsupported(tr!("ir.irgen.111").into_owned())),
            };
            let alias = sub
                .alias
                .as_ref()
                .ok_or_else(|| unsupported(tr!("ir.irgen.112").into_owned()))?;
            if !alias.colnames.is_empty() {
                return Err(unsupported(tr!("ir.irgen.113").into_owned()));
            }

            // 派生表**看不到**同一个查询里的兄弟项(那是 `LATERAL` 的事),所以它的
            // 外层链就是**当前查询的外层链**,不含本层.
            let plan = lower_select(inner, symbols, outers)?;
            // 用浅层分析问 schema:这个计划马上要被接进外层,它的外层引用还等着
            // 外层 `analyze` 校验(那时候外边是什么已经完全确定).
            let columns = plan.analyze_shallow(symbols)?.schemas[plan.root].clone();

            let id = b.splice(plan)?;
            let name = alias.aliasname.clone();
            Ok((
                id,
                Scope::from_range(name.clone(), Some(name), columns, None, symbols)
                    .nested_in(outers.to_vec()),
            ))
        }
        Some(node::Node::RangeFunction(_)) => Err(unsupported(tr!("ir.irgen.114").into_owned())),
        _ => Err(unsupported(tr!("ir.irgen.115").into_owned())),
    }
}

fn lower_join<'s>(
    j: &JoinExpr,
    b: &mut PlanBuilder,
    symbols: &'s SymbolTable,
    outers: &[Scope<'s>],
) -> Result<(NodeId, Scope<'s>)> {
    if j.is_natural {
        return Err(unsupported("NATURAL JOIN"));
    }
    if !j.using_clause.is_empty() {
        // USING 会把连接列**并成一列**,本版的输出 schema 表达不了这个合并,
        // 所以宁可拒绝,也不给一个列重复出现的错误结果.
        return Err(unsupported(
            tr!("ir.irgen.116").into_owned(),
        ));
    }
    if j.join_using_alias.is_some() {
        return Err(unsupported(tr!("ir.irgen.117").into_owned()));
    }

    let join_kind = match JoinType::try_from(j.jointype).unwrap_or(JoinType::Undefined) {
        JoinType::JoinInner => JoinKind::Inner,
        JoinType::JoinLeft => JoinKind::Left,
        other => {
            let what = match other {
                JoinType::JoinRight => "RIGHT JOIN".to_string(),
                JoinType::JoinFull => "FULL JOIN".to_string(),
                JoinType::JoinSemi => "SEMI JOIN".to_string(),
                JoinType::JoinAnti => "ANTI JOIN".to_string(),
                _ => tr!("ir.irgen.118").into_owned(),
            };
            return Err(unsupported(what))
        }
    };

    let larg = j.larg.as_deref().ok_or_else(|| unsupported(tr!("ir.irgen.119").into_owned()))?;
    let rarg = j.rarg.as_deref().ok_or_else(|| unsupported(tr!("ir.irgen.120").into_owned()))?;

    let (left_id, left_scope) = lower_from_item(larg, b, symbols, outers)?;
    let (right_id, mut right_scope) = lower_from_item(rarg, b, symbols, outers)?;

    // 左外连接:右支是**可空侧**,它的 FROM 项不下推谓词(见 `Range::nullable`)
    if join_kind == JoinKind::Left {
        right_scope.mark_nullable();
    }

    let left_width = left_scope.schema().len();
    let scope = Scope::concat(left_scope, right_scope);

    // `ON` 整条降级成**一个**程序:等值键只用来缩小候选集,候选对不对仍由它复核
    let on = match &j.quals {
        Some(expr) => {
            let mut p = ProgBuilder::new();
            let reg = lower_expr(expr, &scope, &mut p)?;
            Some(p.finish(reg))
        }
        None => None,
    };

    // 挑执行策略:先认等值键,再看内侧能不能建索引
    let strategy = match &j.quals {
        Some(expr) => {
            let mut items = Vec::new();
            opt::conjuncts(expr, &mut items);
            let keys = opt::join::equi_keys(&items, &scope, left_width);
            let inner = b.op(right_id).expect(&tr!("ir.irgen.121").into_owned());
            opt::join::join_strategy(keys, inner, scope.symbols())
        }
        // 没有 `ON`(`JOIN ... ON` 之外只在 `FROM a, b` 的叉积里出现)就没有键
        None => JoinStrategy::NestedLoop,
    };

    let id = b.join(left_id, right_id, join_kind, on, strategy)?;
    Ok((id, scope))
}

fn qualified_name(rv: &RangeVar) -> Result<String> {
    if !rv.catalogname.is_empty() {
        return Err(unsupported(format!("{}", tr!("ir.irgen.122", arg0 = 
            rv.catalogname, arg1 =  rv.relname
        ))));
    }
    if rv.schemaname.is_empty() {
        Ok(rv.relname.clone())
    } else {
        Ok(format!("{}.{}", rv.schemaname, rv.relname))
    }
}

// --------------------------------------------------------------- 输出列表

fn lower_target_list(targets: &[Node], scope: &Scope<'_>) -> Result<(Vec<Prog>, Vec<String>)> {
    let mut progs = Vec::new();
    let mut names = Vec::new();

    for target in targets {
        let rt = match &target.node {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => return Err(unsupported(tr!("ir.irgen.123").into_owned())),
        };
        let value = rt
            .val
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.124").into_owned()))?;

        // `*` 与 `t.*`
        if let Some(node::Node::ColumnRef(c)) = &value.node {
            if let Some(qualifier) = star_ref(&c.fields) {
                let mut expanded = 0usize;
                for range in &scope.ranges {
                    if let Some(q) = &qualifier {
                        if !range.matches(q) {
                            continue;
                        }
                    }
                    for (ci, col) in range.columns.iter().enumerate() {
                        let mut p = ProgBuilder::new();
                        let reg = p.col(range.offset + ci);
                        progs.push(p.finish(reg));
                        names.push(col.name.clone());
                        expanded += 1;
                    }
                }
                if expanded == 0 {
                    return Err(DbError::UnknownColumn {
                        name: qualifier.unwrap_or_else(|| "*".to_string()),
                        available: scope.column_names(),
                    });
                }
                continue;
            }
        }

        let mut p = ProgBuilder::new();
        let reg = lower_expr(value, scope, &mut p)?;
        progs.push(p.finish(reg));
        names.push(if rt.name.is_empty() {
            derived_name(value)
        } else {
            rt.name.clone()
        });
    }

    Ok((progs, names))
}

/// 没有 `AS` 时,列名的推导规则:列引用用列名,其它用 `?column?`(跟 PostgreSQL 一致).
fn derived_name(value: &Node) -> String {
    if let Some(node::Node::ColumnRef(c)) = &value.node {
        if let Some(parts) = ref_parts(&c.fields) {
            if let Some(last) = parts.last() {
                return last.clone();
            }
        }
    }
    "?column?".to_string()
}

/// 这个程序是不是"原样取第 i 列".用来省掉恒等的 `Project`.
fn is_identity(progs: &[Prog], names: &[String], schema: &[Column]) -> bool {
    progs.len() == schema.len()
        && progs
            .iter()
            .enumerate()
            .all(|(i, p)| passthrough_column(p) == Some(i))
        && names.iter().zip(schema.iter()).all(|(n, c)| *n == c.name)
}

// ---------------------------------------------------------------- ORDER BY

fn lower_sort_keys(sort_clause: &[Node], scope: &Scope<'_>, ordinals: bool) -> Result<Vec<SortKey>> {
    let mut keys = Vec::with_capacity(sort_clause.len());

    for item in sort_clause {
        let sb = sort_by(item)?;
        let (desc, nulls_first) = sort_direction(sb)?;

        let expr = sb
            .node
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.125").into_owned()))?;

        let mut p = ProgBuilder::new();
        // `ORDER BY 3` 在**输出作用域**里指的是第 3 个输出列,不是常量 3
        let reg = match (ordinals, sort_ordinal_literal(expr)) {
            (true, Some(n)) => ordinal_column(n, scope, &mut p)?,
            _ => lower_expr(expr, scope, &mut p)?,
        };
        keys.push(SortKey {
            prog: p.finish(reg),
            desc,
            nulls_first,
        });
    }

    Ok(keys)
}

/// `ORDER BY` 的第二遍:全部按**输入作用域**解.
///
/// 输出别名与输出列序号都要先换成"目标项自己的表达式"--它们只在输出作用域里
/// 有意义,而这一遍面对的是输入 schema(`p.surname`,`m1.title` 这类限定列名
/// 只有在输入作用域才解得动,测试 11.1 / 12.1 就是这个形状).
fn lower_sort_keys_below(
    sort_clause: &[Node],
    targets: &[Node],
    scope: &Scope<'_>,
) -> Result<Vec<SortKey>> {
    let mut keys = Vec::with_capacity(sort_clause.len());

    for item in sort_clause {
        let sb = sort_by(item)?;
        let (desc, nulls_first) = sort_direction(sb)?;

        let expr = sb
            .node
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.126").into_owned()))?;

        let expr = match sort_ordinal_literal(expr) {
            Some(n) => target_expr(targets, n)?,
            None => substitute_alias(expr, targets),
        };

        let mut p = ProgBuilder::new();
        let reg = lower_expr(expr, scope, &mut p)?;
        keys.push(SortKey {
            prog: p.finish(reg),
            desc,
            nulls_first,
        });
    }

    Ok(keys)
}

/// 排序键是不是一个**整数常量**(`ORDER BY 3`);是就返回它的字面值.
fn sort_ordinal_literal(node: &Node) -> Option<i64> {
    match &node.node {
        Some(node::Node::AConst(c)) => match &c.val {
            Some(Val::Ival(i)) => Some(i.ival as i64),
            Some(Val::Fval(f)) => f.fval.parse::<i64>().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// 排序子句里有没有输出列序号.
fn sort_clause_has_ordinal(sort_clause: &[Node]) -> bool {
    sort_clause.iter().any(|item| {
        sort_by(item)
            .ok()
            .and_then(|sb| sb.node.as_deref())
            .and_then(sort_ordinal_literal)
            .is_some()
    })
}

/// 输出列序号 → 输出作用域里的那一列(1 起).越界在这里就报错.
fn ordinal_column(n: i64, scope: &Scope<'_>, p: &mut ProgBuilder) -> Result<Reg> {
    let width = scope.ranges.first().map(|r| r.columns.len()).unwrap_or(0);
    if n < 1 || n as usize > width {
        return Err(DbError::UnknownColumn {
            name: format!("{}", tr!("ir.irgen.127", arg0 =  n)),
            available: scope.column_names(),
        });
    }
    Ok(p.col(n as usize - 1))
}

/// 输出列序号 → 输出列表里那一项的表达式(1 起).
///
/// `SELECT *` 展不开(一个 `ResTarget` 对应好几列),所以那时只能报错.
fn target_expr<'a>(targets: &'a [Node], n: i64) -> Result<&'a Node> {
    let index = match usize::try_from(n) {
        Ok(n) if n >= 1 => n - 1,
        _ => 0,
    };
    let target = targets.get(index).ok_or_else(|| DbError::UnknownColumn {
        name: format!("{}", tr!("ir.irgen.128", arg0 =  n)),
        available: (1..=targets.len()).map(|i| i.to_string()).collect(),
    })?;

    let rt = match &target.node {
        Some(node::Node::ResTarget(rt)) => rt,
        _ => return Err(unsupported(tr!("ir.irgen.129").into_owned())),
    };
    let value = rt
        .val
        .as_deref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.130").into_owned()))?;

    if let Some(node::Node::ColumnRef(c)) = &value.node {
        if star_ref(&c.fields).is_some() {
            return Err(unsupported(
                tr!("ir.irgen.131").into_owned(),
            ));
        }
    }

    Ok(value)
}

fn sort_by(item: &Node) -> Result<&SortBy> {
    match &item.node {
        Some(node::Node::SortBy(sb)) => Ok(sb),
        _ => Err(unsupported(tr!("ir.irgen.132").into_owned())),
    }
}

/// `(是否降序, NULL 是否排最前)`.
///
/// NULL 的位置是**绝对**的,PostgreSQL 的默认是 ASC → NULLS LAST,
/// DESC → NULLS FIRST.
fn sort_direction(sb: &SortBy) -> Result<(bool, bool)> {
    let desc = match SortByDir::try_from(sb.sortby_dir).unwrap_or(SortByDir::Undefined) {
        SortByDir::SortbyAsc | SortByDir::SortbyDefault => false,
        SortByDir::SortbyDesc => true,
        _ => return Err(unsupported("ORDER BY ... USING")),
    };

    let nulls_first = match SortByNulls::try_from(sb.sortby_nulls).unwrap_or(SortByNulls::Undefined)
    {
        SortByNulls::SortbyNullsFirst => true,
        SortByNulls::SortbyNullsLast => false,
        _ => desc,
    };

    Ok((desc, nulls_first))
}

/// `ORDER BY` 里的裸标识符如果是**显式输出别名**,就换成它对应的表达式.
///
/// 这一步必须在降级**之前**做:换完以后所有排序键都按"分组"降级,于是它们统一
/// 索引 `Aggregate` 的输出 schema,`Sort` 就能干净地放在 `Project` 之下.
/// 不然一个子句里混着别名(索引输出)与分组列(索引聚合输出),一个 `Sort`
/// 节点没法同时满足两种下标.
///
/// 注意 `ResTarget.name` **只有写了 `AS` 才有值**,所以这里只会命中真别名,
/// 不会把 `ORDER BY s.student_id` 误当成别名.
fn substitute_alias<'a>(expr: &'a Node, targets: &'a [Node]) -> &'a Node {
    let name = match &expr.node {
        Some(node::Node::ColumnRef(c)) => match ref_parts(&c.fields) {
            Some(parts) if parts.len() == 1 => parts[0].clone(),
            _ => return expr,
        },
        _ => return expr,
    };

    for target in targets {
        if let Some(node::Node::ResTarget(rt)) = &target.node {
            if rt.name == name {
                if let Some(value) = rt.val.as_deref() {
                    return value;
                }
            }
        }
    }

    expr
}

// ----------------------------------------------------------------- 表达式

/// 降级一个表达式,不允许用上下文定型字面量.
pub(crate) fn lower_expr(node: &Node, scope: &Scope<'_>, p: &mut ProgBuilder) -> Result<Reg> {
    lower_expr_in(node, scope, None, None, p)
}

/// 分组查询的外层:表达式按"每个分组算一次"降级.
fn lower_expr_agg(node: &Node, scope: &Scope<'_>, agg: &mut AggLowering, p: &mut ProgBuilder) -> Result<Reg> {
    lower_expr_in(node, scope, None, Some(agg), p)
}

/// 降级一个表达式,并用 `expected` 解释**无类型字面量**.
///
/// 为什么需要这个:语法树里的 `'2024-01-15'` 只是个字符串,而 `WHERE ts = '...'`
/// 里的 `ts` 是 TIMESTAMP.没有上下文定型,新类型根本没法用.
fn lower_expr_typed(
    node: &Node,
    scope: &Scope<'_>,
    expected: Option<&DataType>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    lower_expr_in(node, scope, expected, None, p)
}

/// 真正的实现.`agg` 为 `Some` 时是**分组查询的外层**:
/// 聚合调用变成对聚合输出的列引用,裸列引用去 `GROUP BY` 列表里找.
fn lower_expr_in(
    node: &Node,
    scope: &Scope<'_>,
    expected: Option<&DataType>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    // 1) 聚合调用
    if let Some(node::Node::FuncCall(call)) = &node.node {
        if let Some(func) = aggregate_func(call) {
            let agg = agg
                .as_deref_mut()
                .ok_or_else(|| unsupported(tr!("ir.irgen.133").into_owned()))?;
            let slot = agg.slot_for(node, call, func, scope)?;
            // 聚合输出 schema = [分组键...] ++ [聚合结果...]
            return Ok(p.col(agg.groups.len() + slot));
        }
    }

    // 2) 整个表达式就是某个分组键?
    if let Some(agg) = agg.as_deref() {
        if let Some(index) = agg.match_group(node, scope) {
            return Ok(p.col(index));
        }
    }

    match &node.node {
        Some(node::Node::ColumnRef(c)) => {
            let parts = ref_parts(&c.fields)
                .ok_or_else(|| unsupported(tr!("ir.irgen.134").into_owned()))?;
            let resolved = scope.resolve(&parts)?;
            // 分组模式下能走到这里,说明这一列既不是分组键,也没被聚合包住
            if agg.is_some() {
                return Err(unsupported(format!("{}", tr!("ir.irgen.135", arg0 = 
                    parts.join(".")
                ))));
            }
            match resolved {
                Resolved::Local(index) => Ok(p.col(index)),
                // 相关子查询:读外层那一行的那一列
                Resolved::Outer { depth, index } => {
                    let ty = scope.outer_type_at(depth, index).ok_or_else(|| {
                        unsupported(format!("{}", tr!("ir.irgen.136", arg0 = 
                            parts.join(".")
                        )))
                    })?;
                    Ok(p.outer(depth, index, ScalarType::Value(ty.family())))
                }
            }
        }

        Some(node::Node::AConst(c)) => lower_const(c, expected, p),

        Some(node::Node::TypeCast(tc)) => {
            // `TYPE 'literal'` / `'literal'::TYPE` 是"给字面量定型":直接按目标类型
            // 解析那个字面量,与过去完全一样.
            //
            // 别的表达式(`x::numeric`)走 `Step::Cast`:只有值模型里真有意义的几组
            // 转换会通过(同族恒等,`INT → NUMERIC`,`NULL`),别的在验证期就被
            // `cast_type` 拒绝.
            let target = column_type(
                tc.type_name
                    .as_ref()
                    .ok_or_else(|| unsupported(tr!("ir.irgen.137").into_owned()))?,
            )?;
            match tc.arg.as_deref().and_then(|a| a.node.as_ref()) {
                Some(node::Node::AConst(c)) => lower_const(c, Some(&target), p),
                Some(_) => {
                    let value = lower_expr_in(
                        tc.arg.as_deref().expect(&tr!("ir.irgen.138").into_owned()),
                        scope,
                        None,
                        agg.as_deref_mut(),
                        p,
                    )?;
                    Ok(p.cast(value, target))
                }
                None => Err(unsupported(tr!("ir.irgen.139").into_owned())),
            }
        }

        // `CURRENT_DATE` 之类的 SQL 内建值(PostgreSQL 解析成 `SQLValueFunction`)
        Some(node::Node::SqlvalueFunction(value)) => lower_sql_value_function(value, p),

        Some(node::Node::AExpr(a)) => lower_aexpr(a, scope, agg.as_deref_mut(), p),

        Some(node::Node::BoolExpr(b)) => lower_boolexpr(b, scope, agg.as_deref_mut(), p),

        Some(node::Node::NullTest(nt)) => {
            if nt.argisrow {
                return Err(unsupported(tr!("ir.irgen.140").into_owned()));
            }
            let negated = match NullTestType::try_from(nt.nulltesttype)
                .unwrap_or(NullTestType::Undefined)
            {
                NullTestType::IsNull => false,
                NullTestType::IsNotNull => true,
                _ => return Err(unsupported(tr!("ir.irgen.141").into_owned())),
            };
            let arg = nt
                .arg
                .as_deref()
                .ok_or_else(|| unsupported(tr!("ir.irgen.142").into_owned()))?;
            let src = lower_expr(arg, scope, p)?;
            Ok(p.is_null(src, negated))
        }
        Some(node::Node::FuncCall(call)) => {
            lower_func_call(call, scope, agg.as_deref_mut(), p)
        }
        Some(node::Node::SubLink(link)) => lower_sublink(link, scope, agg.as_deref_mut(), p),
        Some(node::Node::CaseExpr(case)) => lower_case(case, scope, agg.as_deref_mut(), p),
        Some(node::Node::CoalesceExpr(coal)) => {
            lower_coalesce(coal, scope, agg.as_deref_mut(), p)
        }
        Some(node::Node::Param(_)) => Err(unsupported(tr!("ir.irgen.143").into_owned())),
        Some(node::Node::SetToDefault(_)) => Err(unsupported(tr!("ir.irgen.144").into_owned())),
        Some(node::Node::AStar(_)) => Err(unsupported(tr!("ir.irgen.145").into_owned())),
        _ => Err(unsupported(tr!("ir.irgen.146").into_owned())),
    }
}

/// 内层查询的**外层链**:本层在前,然后一层层往外.
///
/// 这就是相关子查询的全部"绑定":内层降级时解名字会沿着它往外走,解到的每一层都
/// 记成一个 `Step::Outer { depth, index }`(`depth` 是这里的位置).
fn outer_chain<'s>(scope: &Scope<'s>) -> Vec<Scope<'s>> {
    std::iter::once(scope.clone())
        .chain(scope.outers.iter().cloned())
        .collect()
}

/// `SubLink`:标量子查询,`EXISTS`,`IN`/`= ANY`/`= ALL (子查询)`.
///
/// 内层查询降级成一份**自足**的 `Plan`(它自己的 `nodes`/`root`),连同"它引用外层
/// 哪些 schema"一起装进 [`SubPlan`].执行时外层每来一行,内层就重开一份源--拉取式
/// 之下这只是多调一次构建(见 `ir/exec.rs` 的 `ExecCtx`).
///
/// 实测过的语法树形状:
///
/// * `EXISTS` → `EXISTS_SUBLINK`,没有 `testexpr`;
/// * `(SELECT ...)` → `EXPR_SUBLINK`;
/// * `x IN (SELECT ...)` → `ANY_SUBLINK`,`operName` 是**空的**(这正是它与
///   `= ANY` 的区别);`NOT IN` 会在外面套一个 `NOT`;
/// * `x <op> ANY/ALL (SELECT ...)` → `ANY_SUBLINK`/`ALL_SUBLINK` + `operName`.
fn lower_sublink(
    link: &SubLink,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let kind = SubLinkType::try_from(link.sub_link_type).unwrap_or(SubLinkType::Undefined);
    let grouped = agg.is_some();

    let subselect = link
        .subselect
        .as_deref()
        .and_then(|node| node.node.as_ref())
        .ok_or_else(|| unsupported(tr!("ir.irgen.147").into_owned()))?;
    let select = match subselect {
        node::Node::SelectStmt(select) => select,
        _ => return Err(unsupported(tr!("ir.irgen.148").into_owned())),
    };

    // 模式;`ANY` / `ALL` 还要把左边的表达式降级出来
    let mode = match kind {
        SubLinkType::ExistsSublink => SubMode::Exists,
        SubLinkType::ExprSublink => SubMode::Scalar,
        SubLinkType::AnySublink | SubLinkType::AllSublink => {
            let test_node = link
                .testexpr
                .as_deref()
                .ok_or_else(|| unsupported(tr!("ir.irgen.149").into_owned()))?;
            let op = match single_string(&link.oper_name).as_deref() {
                // `IN`(operName 为空)与 `= ANY` 是同一件事
                None | Some("=") => BinOp::Eq,
                Some(other) => binop_of(other)?,
            };
            let mut builder = ProgBuilder::new();
            let reg = lower_expr_in(test_node, scope, None, agg.as_deref_mut(), &mut builder)?;
            SubMode::Quantified {
                test: builder.finish(reg),
                op,
                all: kind == SubLinkType::AllSublink,
            }
        }
        other => {
            return Err(unsupported(format!("{}", tr!("ir.irgen.150", arg0 =  other.as_str_name()))))
        }
    };

    // 内层的外层链.
    //
    // 分组查询里**不做相关子查询**:那种写法在 PostgreSQL 里也要求相关列出现在
    // `GROUP BY` 里,本版没有那套检查,与其猜,不如让外层链为空--内层一旦引用外层
    // 就会以"找不到这一列"报错(响的,不是安静的).
    let chain = match grouped {
        true => Vec::new(),
        false => outer_chain(scope),
    };

    let plan = lower_select(select, scope.symbols, &chain)?;
    // 浅层分析:只要 schema(深一层的外层引用等着外层 `analyze` 校验)
    let schema = plan.analyze_shallow(scope.symbols)?.schemas[plan.root].clone();
    let arity = schema.len();

    if matches!(mode, SubMode::Scalar | SubMode::Quantified { .. }) && arity != 1 {
        return Err(unsupported(format!("{}", tr!("ir.irgen.151", arg0 = 
            arity
        ))));
    }

    let inner = match schema.first() {
        Some(column) => ScalarType::Value(column.ty.family()),
        None => ScalarType::Null,
    };
    let correlated = plan_has_outer(&plan);

    let spec = SubPlan {
        plan,
        mode,
        inner,
        arity,
        outers: chain.iter().map(|scope| scope.schema()).collect(),
        correlated,
    };

    Ok(p.subquery(spec))
}

/// `CASE WHEN ... THEN ... ELSE ... END`.
///
/// 叠成一条**嵌套的 [`Step::Case`](super::expr::Step::Case) 链**,每一层一个分支:
///
/// ```text
/// p:      [cond_1]  Case{ then_1, else = P_2 }
/// P_2:    [cond_2]  Case{ then_2, else = P_3 }
/// ...
/// P_n:    [cond_n]  Case{ then_n, else = 自足程序 }
/// ```
///
/// 于是惰性成立:`cond_i` 只在前 i-1 个条件都不成立时才求值,`THEN`/`ELSE` 的值
/// 只在被选中时才求值--**死分支一行都不算**(`CASE WHEN 1 = 0 THEN 1/0 ELSE 42
/// END` 是 42).最外层那一层直接压进调用方的 `p`,其余各层是新开的子程序.
///
/// 每一层还要回答一个问题:**统一后的类型是 `NUMERIC` 吗**(`INT` 分支要真的
/// 放大).见 [`widen_flag`].
fn lower_case(
    case: &CaseExpr,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    if case.arg.is_some() {
        return Err(unsupported(
            tr!("ir.irgen.152").into_owned(),
        ));
    }

    // 没有 WHEN 分支时整个 `CASE` 就是 ELSE(语法上不该出现,防一手)
    if case.args.is_empty() {
        return match &case.defresult {
            Some(default) => lower_expr_in(default, scope, None, agg.as_deref_mut(), p),
            None => Ok(p.value(Value::Null)),
        };
    }

    // 起点:ELSE 那一段(本身就是一个自足程序)
    let mut else_prog = match &case.defresult {
        Some(default) => branch_prog(default, scope, agg.as_deref_mut())?,
        None => value_prog(Value::Null),
    };

    // 从最后一支往前叠.`i == 0` 那一层落在调用方的 `p` 上,其余各层各自开程序.
    for (i, when) in case.args.iter().enumerate().rev() {
        let arm = match &when.node {
            Some(node::Node::CaseWhen(arm)) => arm,
            _ => return Err(unsupported(tr!("ir.irgen.153").into_owned())),
        };
        let cond_node = arm
            .expr
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.154").into_owned()))?;
        let value_node = arm
            .result
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.155").into_owned()))?;

        if i == 0 {
            // 外层:条件与 `Case` 都写在调用方的程序里
            let cond = lower_expr_in(cond_node, scope, None, agg.as_deref_mut(), p)?;
            let then_prog = branch_prog(value_node, scope, agg.as_deref_mut())?;
            // 探针必须在这两支都降级完之后:它们用到的聚合都已经登记了
            let widen = widen_flag([&then_prog, &else_prog], scope, agg.as_deref());
            return Ok(p.case(cond, then_prog, else_prog, widen));
        }

        let mut inner = ProgBuilder::new();
        let cond = lower_expr_in(cond_node, scope, None, agg.as_deref_mut(), &mut inner)?;
        let then_prog = branch_prog(value_node, scope, agg.as_deref_mut())?;
        let widen = widen_flag([&then_prog, &else_prog], scope, agg.as_deref());
        let dst = inner.case(cond, then_prog, else_prog, widen);
        else_prog = inner.finish(dst);
    }

    // 循环一定会从 `i == 0` 那一层 return;走到这里说明 `args` 是空的
    unreachable!("{}", tr!("ir.irgen.156"))
}

/// `COALESCE(a, b, ...)`:变参,逐个求值,取第一个非 NULL.
///
/// 语法树里它是一个 `CoalesceExpr`(不是 `FuncCall`),而且**不带类型信息**
/// (`coalescetype` 在裸语法树里是 0),所以参数类型统一那件事留给
/// [`crate::ir::expr::Step::Coalesce`] 的校验去做.
///
/// 每个参数都降级成**自足子程序**(与 `CASE` 的分支走同一个 `branch_prog`),于是
/// 惰性是结构性的:`COALESCE(1, 1/0)` 里的除零根本不会被求值.分组查询里参数照样
/// 能是聚合(`COALESCE(SUM(x), 0)`),`agg` 一路传下去就行.
fn lower_coalesce(
    coal: &CoalesceExpr,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let mut args = Vec::with_capacity(coal.args.len());
    for arg in &coal.args {
        args.push(branch_prog(arg, scope, agg.as_deref_mut())?);
    }

    // 放大标志由**参数类型**算出来(与 `CASE` 的 `widen_flag` 同一条理由):
    // `Prog::verify` 会拿它跟 `unify_branches` 的结果对账
    let widen = widen_flag(args.iter(), scope, agg.as_deref());

    Ok(p.coalesce(args, widen))
}

/// 把一个表达式降级成**自足的子程序**(`CASE` 的分支,`AND`/`OR` 的右侧,`COALESCE` 的参数).
fn branch_prog(
    node: &Node,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
) -> Result<Prog> {
    let mut p = ProgBuilder::new();
    let reg = lower_expr_in(node, scope, None, agg.as_deref_mut(), &mut p)?;
    Ok(p.finish(reg))
}

/// 一个常量值构成的程序.
fn value_prog(value: Value) -> Prog {
    let mut p = ProgBuilder::new();
    let reg = p.value(value);
    p.finish(reg)
}

/// 这一层的值要不要放大:这些程序统一后的类型是 `NUMERIC` 吗(`INT` 与 `NUMERIC`
/// 统一成 `NUMERIC`).
///
/// `CASE` 传它的两个分支,`COALESCE` 传全部参数--两者是同一条规则(N 个类型逐个
/// `unify_branches`),所以合成一个函数.
///
/// 类型探针面对的程序输入 schema 与 `Plan::analyze` 用的是同一个:
///
/// * 普通情况就是**当前作用域的扁平 schema**([`Scope::schema`]);
/// * 分组查询(`agg` 是 `Some`)里,程序面对的是 `Aggregate` 的**输出** schema
///   (`[分组键...] ++ [聚合结果...]`),用 [`aggregate_schema`] 现算一份.
///   探针在分支降级**之后**才跑,所以分支用到的聚合都已经登记,这一份 schema 与
///   `analyze` 推出来的在那些下标上逐位相同.
///
/// 探针拿不到类型就给 `false`:那种程序本来就过不了 `analyze`(同一套规则),
/// 而 [`Prog::verify`](super::expr::Prog::verify) 还会核对这个标志--
/// 对不上就报 `IrInvalid`,不会静默少转.
fn widen_flag<'a>(
    progs: impl IntoIterator<Item = &'a Prog>,
    scope: &Scope<'_>,
    agg: Option<&AggLowering>,
) -> bool {
    let input = match agg {
        None => scope.schema(),
        Some(agg) => {
            let names: Vec<String> = agg
                .specs
                .iter()
                .map(|spec| spec.func.name().to_lowercase())
                .collect();
            match aggregate_schema(
                &agg.groups,
                &agg.group_names,
                &agg.specs,
                &names,
                &scope.schema(),
            ) {
                Ok(columns) => columns,
                Err(_) => return false,
            }
        }
    };

    let mut unified: Option<ScalarType> = None;
    for prog in progs {
        let Ok(ty) = prog.verify(&input) else {
            return false;
        };
        unified = match unified {
            None => Some(ty),
            // 统一不了也给 `false`:那种程序过不了 `analyze`,而 `verify` 还会拿这个
            // 标志对账,不会静默少转
            Some(prev) => match unified_branch_type(prev, ty) {
                Some(next) => Some(next),
                None => return false,
            },
        };
    }
    matches!(unified, Some(ScalarType::Value(TypeFamily::Numeric)))
}

/// 标量函数调用.目前只认 `ROUND`.
fn lower_func_call(
    call: &FuncCall,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let name = call
        .funcname
        .last()
        .and_then(|n| match &n.node {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .ok_or_else(|| unsupported(tr!("ir.irgen.157").into_owned()))?;

    // 聚合函数:只有当整个查询是分组查询时才会被 `lower_expr_agg` 接走;
    // 走到这里说明它出现在了不该出现的地方(例如 WHERE 里).
    if aggregate_func(call).is_some() {
        if call.over.is_some() {
            return Err(unsupported(tr!("ir.irgen.158").into_owned()));
        }
        return Err(unsupported(format!("{}", tr!("ir.irgen.159", arg0 = 
            name
        ))));
    }
    if call.over.is_some() {
        return Err(unsupported(tr!("ir.irgen.160").into_owned()));
    }

    match name.as_str() {
        "round" => {
            let args = match call.args.as_slice() {
                [value, digits] => {
                    let v = lower_expr_in(value, scope, None, agg.as_deref_mut(), p)?;
                    let d = lower_expr_in(digits, scope, None, agg.as_deref_mut(), p)?;
                    vec![v, d]
                }
                // `ROUND(x)` 就是舍到整数位
                [value] => {
                    let v = lower_expr_in(value, scope, None, agg.as_deref_mut(), p)?;
                    let zero = p.int(0);
                    vec![v, zero]
                }
                _ => return Err(unsupported(tr!("ir.irgen.161").into_owned())),
            };
            Ok(p.call(ScalarFunc::Round, args))
        }

        "upper" => {
            let args = match call.args.as_slice() {
                [value] => vec![lower_expr_in(value, scope, None, agg.as_deref_mut(), p)?],
                _ => return Err(unsupported(tr!("ir.irgen.162").into_owned())),
            };
            Ok(p.call(ScalarFunc::Upper, args))
        }

        "length" => {
            let args = match call.args.as_slice() {
                [text] => vec![lower_expr_in(text, scope, None, agg.as_deref_mut(), p)?],
                _ => return Err(unsupported(tr!("ir.irgen.163").into_owned())),
            };
            Ok(p.call(ScalarFunc::Length, args))
        }

        "left" => {
            let args = match call.args.as_slice() {
                [text, count] => vec![
                    lower_expr_in(text, scope, None, agg.as_deref_mut(), p)?,
                    lower_expr_in(count, scope, None, agg.as_deref_mut(), p)?,
                ],
                _ => return Err(unsupported(tr!("ir.irgen.164").into_owned())),
            };
            Ok(p.call(ScalarFunc::Left, args))
        }

        // `CONCAT_WS(sep, a, b, ...)`:变参,逐个数降级
        "concat_ws" => {
            if call.args.is_empty() {
                return Err(unsupported(tr!("ir.irgen.165").into_owned()));
            }
            let mut args = Vec::with_capacity(call.args.len());
            for arg in &call.args {
                args.push(lower_expr_in(arg, scope, None, agg.as_deref_mut(), p)?);
            }
            Ok(p.call(ScalarFunc::ConcatWs, args))
        }

        // `EXTRACT(YEAR FROM ts)`:语法树里是 `FuncCall("extract", [字段, 值])`,
        // 字段是 `A_Const` 的字符串(`makeStringConst`).
        "extract" => {
            let (field, value) = match call.args.as_slice() {
                [field, value] => (field, value),
                _ => {
                    return Err(unsupported(
                        tr!("ir.irgen.166").into_owned(),
                    ))
                }
            };
            let field = const_string(field)
                .ok_or_else(|| unsupported(tr!("ir.irgen.167").into_owned()))?;
            if !field.eq_ignore_ascii_case("year") {
                return Err(unsupported(format!("{}", tr!("ir.irgen.168", arg0 = 
                    field
                ))));
            }
            let value = lower_expr_in(value, scope, None, agg.as_deref_mut(), p)?;
            Ok(p.call(ScalarFunc::ExtractYear, vec![value]))
        }

        other => Err(unsupported(format!("{}", tr!("ir.irgen.169", arg0 =  other)))),
    }
}

/// `CURRENT_DATE` 之类的 SQL 内建值.
///
/// 本版只认 `CURRENT_DATE`,产出一个 `DATE`(定宽的 `YYYY-MM-DD` 规范文本,于是
/// 字典序就是时间序).本版没有任何日期运算,所以它只用来当值取出来,或用
/// `EXTRACT(YEAR FROM ...)` 取年份.
fn lower_sql_value_function(f: &SqlValueFunction, p: &mut ProgBuilder) -> Result<Reg> {
    let op =
        SqlValueFunctionOp::try_from(f.op).unwrap_or(SqlValueFunctionOp::SqlvalueFunctionOpUndefined);
    match op {
        SqlValueFunctionOp::SvfopCurrentDate => Ok(p.value(Value::Date(current_date()))),
        other => Err(unsupported(format!("{}", tr!("ir.irgen.170", arg0 = 
            other.as_str_name()
        )))),
    }
}

/// 今天的日期,规范成 `YYYY-MM-DD`(本地时钟;由 [`civil_date_from_days`] 换算).
fn current_date() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil_date_from_days(days);
    format!("{:04}-{:02}-{:02}", year, month, day)
}

/// 从 Unix 纪元起的天数算出公历 `(年, 月, 日)`(Howard Hinnant 的 `civil_from_days`).
///
/// 只要"今天是几号"这一件事,不值得引入日期库;推导过程全是整数运算,与
/// `1970-01-01` 对应 0 的约定一起构成可单测的事实.
pub fn civil_date_from_days(days: i64) -> (i32, u32, u32) {
    // 把纪元挪到 0000-03-01(这样闰年规则落在同一年内的末尾)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11],3 月是 0
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (
        (y + i64::from(month <= 2)) as i32,
        month as u32,
        day as u32,
    )
}

/// 这是个聚合函数调用吗.窗口函数(带 `OVER`)**不算**--那是另一回事.
fn aggregate_func(call: &FuncCall) -> Option<AggFunc> {
    if call.over.is_some() {
        return None;
    }
    let name = call.funcname.last().and_then(|n| match &n.node {
        Some(node::Node::String(s)) => Some(s.sval.as_str()),
        _ => None,
    })?;
    match name {
        "count" => Some(AggFunc::Count),
        "sum" => Some(AggFunc::Sum),
        "avg" => Some(AggFunc::Avg),
        "min" => Some(AggFunc::Min),
        "max" => Some(AggFunc::Max),
        _ => None,
    }
}

/// 这棵表达式子树里有没有聚合调用.
fn contains_aggregate(node: &Node) -> bool {
    match &node.node {
        Some(node::Node::FuncCall(call)) => {
            aggregate_func(call).is_some()
                || call.args.iter().any(contains_aggregate)
                || call.agg_order.iter().any(contains_aggregate)
        }
        Some(node::Node::AExpr(a)) => {
            a.lexpr.as_deref().is_some_and(contains_aggregate)
                || a.rexpr.as_deref().is_some_and(contains_aggregate)
        }
        Some(node::Node::BoolExpr(b)) => b.args.iter().any(contains_aggregate),
        Some(node::Node::CaseExpr(c)) => {
            c.args.iter().any(contains_aggregate)
                || c.defresult.as_deref().is_some_and(contains_aggregate)
        }
        // `COALESCE` 的参数里也可能有聚合(`COALESCE(COUNT(*), 0)`),漏掉这一支会让
        // "是不是分组查询"判错,于是那个聚合被当成"出现在了不能出现的位置"
        Some(node::Node::CoalesceExpr(c)) => c.args.iter().any(contains_aggregate),
        // `CASE WHEN <条件> THEN <值>` 的**条件和值都可能是聚合**:
        // `CASE WHEN COUNT(*) > 0 THEN ...` 就是一个分组查询.漏掉这一支会让"是不是
        // 分组查询"判错,于是聚合被当成"出现在了不能出现的位置".
        Some(node::Node::CaseWhen(arm)) => {
            arm.expr.as_deref().is_some_and(contains_aggregate)
                || arm.result.as_deref().is_some_and(contains_aggregate)
        }
        Some(node::Node::NullTest(n)) => n.arg.as_deref().is_some_and(contains_aggregate),
        Some(node::Node::TypeCast(t)) => t.arg.as_deref().is_some_and(contains_aggregate),
        Some(node::Node::ResTarget(r)) => r.val.as_deref().is_some_and(contains_aggregate),
        // `ORDER BY COUNT(*)` 同样是分组查询(排序键也是"外层"表达式)
        Some(node::Node::SortBy(s)) => s.node.as_deref().is_some_and(contains_aggregate),
        _ => false,
    }
}

/// 分组查询降级时的状态.
///
/// **一趟到底**:外层的表达式边降级边登记聚合,而不是先走一遍收集再走一遍改写.
/// 两趟写法要求两次遍历的访问顺序完全一致,错了就会**静默串位**;用 AST 节点地址
/// 认同一个聚合,一趟就够,也不会重复登记同一个 `COUNT(*)`.
#[derive(Default)]
struct AggLowering {
    /// `GROUP BY` 的表达式,按输入行求值.
    groups: Vec<Prog>,
    /// 分组键的输出列名(仅用于展示/给聚合节点起名).
    group_names: Vec<String>,
    /// 已登记的聚合.
    specs: Vec<AggSpec>,
    /// 哪个 AST 聚合调用节点对应哪个槽位(按节点地址认).
    slots: Vec<(*const Node, usize)>,
}

impl AggLowering {
    /// 登记一个聚合,返回它的槽位.同一个 AST 节点只登记一次.
    fn slot_for(
        &mut self,
        node: &Node,
        call: &FuncCall,
        func: AggFunc,
        scope: &Scope<'_>,
    ) -> Result<usize> {
        let address = node as *const Node;
        if let Some((_, slot)) = self.slots.iter().find(|(n, _)| std::ptr::eq(*n, address)) {
            return Ok(*slot);
        }

        if !call.agg_order.is_empty() {
            return Err(unsupported(tr!("ir.irgen.171").into_owned()));
        }

        // `FILTER (WHERE ...)` 按**输入行**求值(不是分组外层),所以走 `lower_expr`:
        // 里面的聚合调用因此也会被挡在"不能出现的位置"上,与 PostgreSQL 一致.
        let filter = match call.agg_filter.as_deref() {
            Some(node) => {
                let mut p = ProgBuilder::new();
                let reg = lower_expr(node, scope, &mut p)?;
                Some(p.finish(reg))
            }
            None => None,
        };

        let arg = if call.agg_star {
            None
        } else {
            match call.args.as_slice() {
                [only] => {
                    let mut p = ProgBuilder::new();
                    let reg = lower_expr(only, scope, &mut p)?;
                    Some(p.finish(reg))
                }
                [] => return Err(unsupported(format!("{}", tr!("ir.irgen.172", arg0 =  func.name())))),
                _ => {
                    return Err(unsupported(format!("{}", tr!("ir.irgen.173", arg0 = 
                        func.name()
                    ))))
                }
            }
        };

        let slot = self.specs.len();
        self.specs.push(AggSpec {
            func,
            arg,
            distinct: call.agg_distinct,
            filter,
        });
        self.slots.push((address, slot));
        Ok(slot)
    }

    /// 这个表达式整体是不是某个分组键;是就返回它的下标.
    fn match_group(&self, node: &Node, scope: &Scope<'_>) -> Option<usize> {
        if self.groups.is_empty() {
            return None;
        }
        // 按输入行降级一次再比程序.含聚合的表达式这里会失败,于是"不算分组键".
        let mut p = ProgBuilder::new();
        let reg = lower_expr(node, scope, &mut p).ok()?;
        let prog = p.finish(reg);
        self.groups.iter().position(|group| *group == prog)
    }
}

/// 算 `Aggregate` 节点的输出 schema(与 `Op::Aggregate` 的 analyze 规则一致).
fn aggregate_schema(
    groups: &[Prog],
    group_names: &[String],
    specs: &[AggSpec],
    agg_names: &[String],
    input: &[Column],
) -> Result<Vec<Column>> {
    let mut columns = Vec::with_capacity(groups.len() + specs.len());

    for (prog, name) in groups.iter().zip(group_names) {
        columns.push(project_column(prog, name, input)?);
    }

    for (spec, name) in specs.iter().zip(agg_names) {
        let arg = match &spec.arg {
            Some(prog) => Some(prog.verify(input)?),
            None => None,
        };
        let family = match spec.func.result_type(arg) {
            ScalarType::Value(family) => family,
            _ => TypeFamily::Int,
        };
        columns.push(Column::new(name.clone(), family.default_type()));
    }

    Ok(columns)
}

/// `SELECT` 列表,按**分组**求值.
fn lower_target_list_agg(
    targets: &[Node],
    scope: &Scope<'_>,
    agg: &mut AggLowering,
) -> Result<(Vec<Prog>, Vec<String>)> {
    let mut progs = Vec::new();
    let mut names = Vec::new();

    for target in targets {
        let rt = match &target.node {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => return Err(unsupported(tr!("ir.irgen.174").into_owned())),
        };
        let value = rt
            .val
            .as_deref()
            .ok_or_else(|| unsupported(tr!("ir.irgen.175").into_owned()))?;

        // 分组查询里 `*` 没法一般化地展开(要靠 GROUP BY 判断哪些列合法),
        // 与其猜,不如明说
        if let Some(node::Node::ColumnRef(c)) = &value.node {
            if star_ref(&c.fields).is_some() {
                return Err(unsupported(
                    tr!("ir.irgen.176").into_owned(),
                ));
            }
        }

        let mut p = ProgBuilder::new();
        let reg = lower_expr_agg(value, scope, agg, &mut p)?;
        progs.push(p.finish(reg));
        names.push(if rt.name.is_empty() {
            derived_name(value)
        } else {
            rt.name.clone()
        });
    }

    Ok((progs, names))
}

/// 降级一个字面量.
///
/// `expected` 是上下文给出的目标类型:`'Alice'` 在没有上下文时默认 TEXT,在
/// `WHERE name = 'Alice'` 里则按 `name` 的类型解释.
fn lower_const(c: &AConst, expected: Option<&DataType>, p: &mut ProgBuilder) -> Result<Reg> {
    if c.isnull {
        // `NULL` 关键字:无类型,靠上下文定型(比较,INSERT 的目标列等)
        return Ok(p.value(Value::Null));
    }

    // 有上下文时,按**隐式转换规则**把字面量定成目标类型:
    //
    // * 字符串字面量是 PostgreSQL 说的"未知类型",可以定成任何类型
    //   (`name = 'Alice'`,`ts > '2000-01-01'`,`'5'::int` 都靠这条);
    // * 整数只能落到 INT 或 NUMERIC(int → numeric 是隐式转换);
    // * 小数只能落到 NUMERIC.
    //
    // 落不下去的(例如 `text_col = 5`)**不硬塞**,让它按本来类型去比,
    // 于是 `verify` 会报出"两侧类型不同"--这正是 PostgreSQL 的行为
    // (`operator does not exist: text = integer`).
    if let Some(ty) = expected {
        let family = ty.family();
        let coercible = match &c.val {
            Some(Val::Sval(_)) => true,
            Some(Val::Ival(_)) => matches!(family, TypeFamily::Int | TypeFamily::Numeric),
            Some(Val::Fval(f)) => {
                matches!(family, TypeFamily::Numeric)
                    || (matches!(family, TypeFamily::Int) && f.fval.parse::<i32>().is_ok())
            }
            _ => false,
        };

        if coercible {
            if let Some(text) = literal_text(c) {
                let value = ty.parse(&text).map_err(|e| at_value(&text, e))?;
                return Ok(p.value(value));
            }
        }
    }

    match &c.val {
        Some(Val::Ival(i)) => Ok(p.int(i.ival)),

        // 没有上下文时,pg_query 把**超出 i32 的整数也塞进 Fval**:`SELECT
        // -2147483648` 是合法的 i32 却落在 Fval 里,而 `SELECT 2147483648` 才是真
        // 超范围.所以先按整数试一次--直接"见 Fval 就拒"会误杀 i32::MIN.
        Some(Val::Fval(f)) => match f.fval.parse::<i32>() {
            Ok(v) => Ok(p.int(v)),
            // 落不进 i32 的裸小数就是 NUMERIC -- 与 PostgreSQL 一致
            // (`SELECT 2.65` 的类型就是 numeric).
            Err(_) => {
                // 落不进 i32 的裸小数就是**不限定**的 NUMERIC
                let value = DataType::NumericAny
                    .parse(&f.fval)
                    .map_err(|e| at_value(&f.fval, e))?;
                Ok(p.value(value))
            }
        },

        // 没有上下文的字符串字面量默认 TEXT
        Some(Val::Sval(s)) => {
            let value = DataType::Text.parse(&s.sval).map_err(|e| at_value(&s.sval, e))?;
            Ok(p.value(value))
        }

        Some(Val::Boolval(b)) => Ok(p.boolean(b.boolval)),
        Some(Val::Bsval(_)) => Err(unsupported(tr!("ir.irgen.177").into_owned())),
        None => Err(unsupported(tr!("ir.irgen.178").into_owned())),
    }
}

/// 字面量的文本形式(整数也转成文本,好统一按目标类型再解析一次).
fn literal_text(c: &AConst) -> Option<String> {
    match &c.val {
        Some(Val::Ival(i)) => Some(i.ival.to_string()),
        Some(Val::Fval(f)) => Some(f.fval.clone()),
        Some(Val::Sval(s)) => Some(s.sval.clone()),
        _ => None,
    }
}

/// 给字面量的解析失败补上"这是字面量"的上下文.
fn at_value(text: &str, e: DbError) -> DbError {
    match e {
        DbError::BadValue { ty, reason, .. } => DbError::BadValue {
            column: None,
            ty,
            value: format!("{}", tr!("ir.irgen.179", arg0 =  text)),
            reason,
        },
        other => other,
    }
}

/// 如果这个节点是个能解析的裸列引用,返回它的列类型.上下文定型要用.
fn bare_column_type(node: &Node, scope: &Scope<'_>) -> Option<DataType> {
    let c = match &node.node {
        Some(node::Node::ColumnRef(c)) => c,
        _ => return None,
    };
    let parts = ref_parts(&c.fields)?;
    match scope.resolve(&parts).ok()? {
        Resolved::Local(index) => scope.column_type_at(index),
        Resolved::Outer { depth, index } => scope.outer_type_at(depth, index),
    }
}

fn lower_aexpr(
    a: &AExpr,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let kind = AExprKind::try_from(a.kind).unwrap_or(AExprKind::Undefined);

    // `IN` 与 `BETWEEN` 的右边是一个 `List`(不是普通二元运算),单独走
    match kind {
        AExprKind::AexprIn => return lower_in(a, scope, agg.as_deref_mut(), p),
        AExprKind::AexprBetween | AExprKind::AexprNotBetween => {
            return lower_between(a, kind, scope, agg.as_deref_mut(), p)
        }
        // `LIKE` / `ILIKE` 与正则一样是"二元运算符",只是运算符名不同
        AExprKind::AexprOp | AExprKind::AexprLike | AExprKind::AexprIlike => {}
        other => {
            return Err(unsupported(match other {
                AExprKind::AexprSimilar => "SIMILAR TO".to_string(),
                AExprKind::AexprNullif => "NULLIF".to_string(),
                AExprKind::AexprDistinct | AExprKind::AexprNotDistinct => {
                    "IS [NOT] DISTINCT FROM".to_string()
                }
                AExprKind::AexprOpAny | AExprKind::AexprOpAll => {
                    tr!("ir.irgen.180").into_owned()
                }
                _ => format!("{}", tr!("ir.irgen.181", arg0 = 
                    single_string(&a.name).unwrap_or_default()
                )),
            }))
        }
    }

    let op_name = single_string(&a.name).ok_or_else(|| unsupported(tr!("ir.irgen.182").into_owned()))?;

    match (a.lexpr.as_deref(), a.rexpr.as_deref()) {
        // 二元
        (Some(l), Some(r)) => {
            let op = binop_of(&op_name)?;

            // 比较运算:一侧是裸列引用时,用**它的列类型**给另一侧的字面量定型.
            // 没有这一步,`WHERE ts = '2024-01-15'` 里的字面量会默认成 TEXT,
            // 与 TIMESTAMP 列比较时报类型不符.
            let (left, right) = if op.is_comparison() {
                match (bare_column_type(l, scope), bare_column_type(r, scope)) {
                    (Some(ty), None) => {
                        let left = lower_expr_in(l, scope, None, agg.as_deref_mut(), p)?;
                        let right = lower_expr_in(r, scope, Some(&ty), agg.as_deref_mut(), p)?;
                        (left, right)
                    }
                    (None, Some(ty)) => {
                        let left = lower_expr_in(l, scope, Some(&ty), agg.as_deref_mut(), p)?;
                        let right = lower_expr_in(r, scope, None, agg.as_deref_mut(), p)?;
                        (left, right)
                    }
                    _ => (
                        lower_expr_in(l, scope, None, agg.as_deref_mut(), p)?,
                        lower_expr_in(r, scope, None, agg.as_deref_mut(), p)?,
                    ),
                }
            } else {
                (
                    lower_expr_in(l, scope, None, agg.as_deref_mut(), p)?,
                    lower_expr_in(r, scope, None, agg.as_deref_mut(), p)?,
                )
            };

            Ok(p.binary(op, left, right))
        }
        // 一元
        (None, Some(r)) => {
            let operand = lower_expr_in(r, scope, None, agg.as_deref_mut(), p)?;
            match op_name.as_str() {
                "-" => Ok(p.unary(UnOp::Neg, operand)),
                "+" => Ok(operand),
                other => Err(unsupported(format!("{}", tr!("ir.irgen.183", arg0 =  other)))),
            }
        }
        _ => Err(unsupported(format!("{}", tr!("ir.irgen.184", arg0 =  op_name)))),
    }
}

/// `x IN (a, b, c)` 与 `x NOT IN (a, b, c)`.
///
/// 语法树里这是一个 [`AExpr`] (`kind = AexprIn`,`name` 是 `=` 或 `<>`),右边是
/// `List`.折成 `x = a OR x = b ...`(`NOT IN` 折成 `<> ... AND ...`):Kleene 三值
/// 逻辑下这与 PostgreSQL **完全等价**--`x NOT IN (1, NULL)` 也该是 UNKNOWN.
///
/// 第 2 项起都放进**自足子程序**,用 [`Step::Or`](super::expr::Step::Or) /
/// `And` 串起来,于是与 `AND`/`OR` 一样短路:`x IN (1, 1/0)` 只要第一项就命中.
///
/// `x IN (SELECT ...)` 是 `SubLink`,不走这里.
fn lower_in(
    a: &AExpr,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let negated = match single_string(&a.name).as_deref() {
        Some("=") => false,
        Some("<>") => true,
        other => {
            return Err(unsupported(format!("{}", tr!("ir.irgen.185", arg0 = 
                other.unwrap_or("?")
            ))))
        }
    };

    let lhs = a.lexpr.as_deref().ok_or_else(|| unsupported(tr!("ir.irgen.186").into_owned()))?;
    let items = match a.rexpr.as_deref().and_then(|n| n.node.as_ref()) {
        Some(node::Node::List(list)) => &list.items,
        _ => return Err(unsupported(tr!("ir.irgen.187").into_owned())),
    };
    if items.is_empty() {
        return Err(unsupported(tr!("ir.irgen.188").into_owned()));
    }

    // 用左边的列类型给列表元素定型(`ts IN ('2024-01-15')`)
    let expected = bare_column_type(lhs, scope);

    let cmp = if negated { BinOp::Ne } else { BinOp::Eq };

    // 第一项直接写在 `p` 里(它总要被算),其余各项各成一个子程序
    let mut iter = items.iter();
    let first = iter.next().expect(&tr!("ir.irgen.189").into_owned());
    let left = lower_expr_in(lhs, scope, None, agg.as_deref_mut(), p)?;
    let right = lower_expr_in(first, scope, expected.as_ref(), agg.as_deref_mut(), p)?;
    let mut acc = p.binary(cmp, left, right);

    for item in iter {
        let mut sub = ProgBuilder::new();
        let left = lower_expr_in(lhs, scope, None, agg.as_deref_mut(), &mut sub)?;
        let right = lower_expr_in(item, scope, expected.as_ref(), agg.as_deref_mut(), &mut sub)?;
        let reg = sub.binary(cmp, left, right);
        let item_prog = sub.finish(reg);
        acc = if negated {
            p.and(acc, item_prog)
        } else {
            p.or(acc, item_prog)
        };
    }

    Ok(acc)
}

/// `x BETWEEN lo AND hi` / `x NOT BETWEEN lo AND hi`.
///
/// 语法树里是 [`AExpr`] (`kind = AexprBetween`/`AexprNotBetween`),右边是
/// `List[lo, hi]`.折成 `(x >= lo) AND (x <= hi)`;`NOT BETWEEN` 折成
/// `(x < lo) OR (x > hi)`--三值逻辑下与 PostgreSQL 一样,短路也一样(下界就能
/// 定胜负时上界不求值).
fn lower_between(
    a: &AExpr,
    kind: AExprKind,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let negated = kind == AExprKind::AexprNotBetween;

    let lhs = a
        .lexpr
        .as_deref()
        .ok_or_else(|| unsupported(tr!("ir.irgen.190").into_owned()))?;
    let bounds = match a.rexpr.as_deref().and_then(|n| n.node.as_ref()) {
        Some(node::Node::List(list)) if list.items.len() == 2 => &list.items,
        _ => return Err(unsupported(tr!("ir.irgen.191").into_owned())),
    };

    let expected = bare_column_type(lhs, scope);

    let low_op = if negated { BinOp::Lt } else { BinOp::Ge };
    let high_op = if negated { BinOp::Gt } else { BinOp::Le };

    // 下界直接写在 `p` 里;上界是一个自足子程序(下界已经不成立时不必算它)
    let low = {
        let left = lower_expr_in(lhs, scope, None, agg.as_deref_mut(), p)?;
        let right = lower_expr_in(&bounds[0], scope, expected.as_ref(), agg.as_deref_mut(), p)?;
        p.binary(low_op, left, right)
    };
    let high = {
        let mut sub = ProgBuilder::new();
        let left = lower_expr_in(lhs, scope, None, agg.as_deref_mut(), &mut sub)?;
        let right = lower_expr_in(&bounds[1], scope, expected.as_ref(), agg.as_deref_mut(), &mut sub)?;
        let reg = sub.binary(high_op, left, right);
        sub.finish(reg)
    };

    Ok(if negated {
        p.or(low, high)
    } else {
        p.and(low, high)
    })
}

fn lower_boolexpr(
    b: &BoolExpr,
    scope: &Scope<'_>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let kind = BoolExprType::try_from(b.boolop).unwrap_or(BoolExprType::Undefined);

    match kind {
        // `AND`/`OR` 走 `Step::And`/`Step::Or`:左侧能定胜负时右侧**不求值**.
        // 第 1 个参数留在 `p` 里,其余每个都是自足子程序.
        BoolExprType::AndExpr | BoolExprType::OrExpr => {
            let is_and = kind == BoolExprType::AndExpr;
            let mut iter = b.args.iter();
            let first = iter.next().ok_or_else(|| unsupported(tr!("ir.irgen.192").into_owned()))?;
            let mut acc = lower_expr_in(first, scope, None, agg.as_deref_mut(), p)?;

            for arg in iter {
                let right = branch_prog(arg, scope, agg.as_deref_mut())?;
                acc = if is_and {
                    p.and(acc, right)
                } else {
                    p.or(acc, right)
                };
            }

            Ok(acc)
        }

        BoolExprType::NotExpr => {
            if b.args.len() != 1 {
                return Err(unsupported(tr!("ir.irgen.193").into_owned()));
            }
            let inner = lower_expr_in(&b.args[0], scope, None, agg.as_deref_mut(), p)?;
            Ok(p.unary(UnOp::Not, inner))
        }

        _ => Err(unsupported(tr!("ir.irgen.194").into_owned())),
    }
}

fn binop_of(op: &str) -> Result<BinOp> {
    Ok(match op {
        "=" => BinOp::Eq,
        "<>" | "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        // 文本连接:PostgreSQL 把 `||` 也叫"运算符",语法树里与算术同级
        "||" => BinOp::Concat,
        // `LIKE` 族在语法树里是这几个内部运算符名(pg_query 的 `AexprLike` /
        // `AexprIlike` 分支)
        "~~" => BinOp::Like,
        "!~~" => BinOp::NotLike,
        "~~*" => BinOp::ILike,
        "!~~*" => BinOp::NotILike,
        // 正则匹配是普通运算符
        "~" => BinOp::Regex,
        "!~" => BinOp::NotRegex,
        "~*" => BinOp::IRegex,
        "!~*" => BinOp::NotIRegex,
        other => return Err(unsupported(format!("{}", tr!("ir.irgen.195", arg0 =  other)))),
    })
}

/// `ColumnRef.fields` → 一串字符串;含 `A_Star` 或别的节点则 `None`.
pub(crate) fn ref_parts(fields: &[Node]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        match &field.node {
            Some(node::Node::String(s)) => out.push(s.sval.clone()),
            _ => return None,
        }
    }
    Some(out)
}

/// 识别 `*`(返回 `Some(None)`)与 `t.*`(返回 `Some(Some("t"))`).
fn star_ref(fields: &[Node]) -> Option<Option<String>> {
    match fields.last().and_then(|f| f.node.as_ref()) {
        Some(node::Node::AStar(_)) => match fields.len() {
            1 => Some(None),
            2 => match &fields[0].node {
                Some(node::Node::String(s)) => Some(Some(s.sval.clone())),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn single_string(items: &[Node]) -> Option<String> {
    match items {
        [one] => match &one.node {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// 一个"常量字符串"节点 → 它的文本.
///
/// `EXTRACT(YEAR FROM x)` 的字段是 `makeStringConst` 造出来的 `A_Const`(`Sval`),
/// 但语法树里也可能直接是 `String` 节点,两种都认.
fn const_string(node: &Node) -> Option<String> {
    match &node.node {
        Some(node::Node::AConst(c)) => match &c.val {
            Some(Val::Sval(s)) => Some(s.sval.clone()),
            _ => None,
        },
        Some(node::Node::String(s)) => Some(s.sval.clone()),
        _ => None,
    }
}

fn unsupported(feature: impl Into<String>) -> DbError {
    DbError::Unsupported {
        feature: feature.into(),
    }
}

// ------------------------------------------------------------------ 作用域

/// 一个列引用解出来的位置.
///
/// 相关子查询的机制就落在这一个枚举上:解到 [`Resolved::Outer`] 说明这个引用
/// **不是本查询的行**,而是外面某一层--降级时它变成一个 `Step::Outer`,执行时从
/// 外层行栈里取(栈由 [`crate::ir::exec`] 在求值子查询时压入).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// 本查询 FROM 的扁平 schema 里的下标.
    Local(usize),
    /// 外层:隔了几层(`1` 是直接外层)+ 那一层的扁平下标.
    Outer {
        /// 隔了几层,从 1 开始.
        depth: u16,
        /// 那一层的扁平下标.
        index: usize,
    },
}

/// 一个 FROM 项:名字,别名,以及它的列在扁平 schema 里的位置.
#[derive(Debug, Clone)]
pub(crate) struct Range {
    /// 限定名(`study.people` 或 `f`).
    pub(crate) name: String,
    /// 别名(`AS t`).
    alias: Option<String>,
    /// 这一项的列在扁平 schema 里的起始下标.
    pub(crate) offset: usize,
    /// 列定义.
    pub(crate) columns: Vec<Column>,
    /// 这一项对应的基表 `Scan` 节点.
    ///
    /// `None` 表示"不是 FROM 项"--现在只有 `ORDER BY` 的输出作用域是这种
    /// (它只是拿来解名字的,没有基表).
    pub(crate) scan: Option<NodeId>,
    /// 这一项落在某个 `LEFT JOIN` 的**可空侧**吗.
    ///
    /// 可空侧的列会被外连接补出来的 NULL 污染,于是"把只引用这一项的谓词推到它的
    /// 基表 `Scan`"不再保语义:`WHERE b.y IS NULL` 就是反例--补出来的 NULL 也满足
    /// 它,推下去那些行就没了.所以这些项一律不下推:宁可少优化,也不改语义.
    pub(crate) nullable: bool,
}

impl Range {
    /// 查询里写的 `qualifier` 能不能指向这一项.
    fn matches(&self, qualifier: &str) -> bool {
        if self.alias.as_deref() == Some(qualifier) {
            return true;
        }
        if self.name == qualifier {
            return true;
        }
        // `study.people.surname` 这种三段引用:用表名的最后一段匹配
        self.name.rsplit('.').next() == Some(qualifier)
    }

    /// 用于报错的限定名.
    fn qualified(&self) -> String {
        self.alias.clone().unwrap_or_else(|| self.name.clone())
    }

    /// 扁平下标是这一项的列吗.
    pub(crate) fn contains(&self, index: usize) -> bool {
        index >= self.offset && index < self.offset + self.columns.len()
    }
}

/// 降级期的名字作用域.顺序必须与 [`Plan::analyze`] 推导的 schema 一致.
///
/// # 外层链与符号表
///
/// 子查询就是"再开一个查询",所以降级一个查询时还需要两样东西:
///
/// * `outers`:外面的查询作用域(`outers[0]` 是**直接外层**)--相关子查询里的
///   列名从这里解,解出来的是 [`Resolved::Outer`] (隔了几层 + 那一层的下标);
/// * `symbols`:符号表--内层子查询里的表名还得查它.
///
/// 两者都跟着作用域走:作用域本来就是"降级一个查询时要的那些上下文".
#[derive(Debug, Clone)]
pub(crate) struct Scope<'s> {
    pub(crate) ranges: Vec<Range>,
    /// 外面的查询作用域,一层层往外.
    outers: Vec<Scope<'s>>,
    /// 符号表.
    symbols: &'s SymbolTable,
}

impl<'s> Scope<'s> {
    fn from_range(
        name: String,
        alias: Option<String>,
        columns: Vec<Column>,
        scan: Option<NodeId>,
        symbols: &'s SymbolTable,
    ) -> Self {
        Scope {
            ranges: vec![Range {
                name,
                alias,
                offset: 0,
                columns,
                scan,
                nullable: false,
            }],
            outers: Vec::new(),
            symbols,
        }
    }

    /// 没有 FROM 项的作用域:`SELECT 1 + 1`,`INSERT` 的值表达式用它.
    fn empty(symbols: &'s SymbolTable) -> Self {
        Scope {
            ranges: Vec::new(),
            outers: Vec::new(),
            symbols,
        }
    }

    /// 把这个作用域当成"某一层外层"挂上(降级内层查询前用).
    fn nested_in(mut self, outers: Vec<Scope<'s>>) -> Self {
        self.outers = outers;
        self
    }

    /// 把另一个作用域接在后面(连接的输出 = 左 ++ 右).
    fn concat(mut self, other: Scope<'s>) -> Self {
        for range in other.ranges {
            let offset = self.ranges.iter().map(|r| r.columns.len()).sum();
            self.ranges.push(Range { offset, ..range });
        }
        self
    }

    /// 把**现在**这些项都标成可空侧(`LEFT JOIN` 的右支用它,见 [`Range::nullable`]).
    fn mark_nullable(&mut self) {
        for range in &mut self.ranges {
            range.nullable = true;
        }
    }

    /// `cols` 全部落在**一个可以下推的** FROM 项里吗;是就返回那一项的下标.
    ///
    /// 这几种情况都给 `None`(谓词留在 `Filter` 里):
    ///
    /// * 空集合(常量谓词,`WHERE 1 = 1`)--推下去也没收益;
    /// * 列横跨多个 FROM 项--那只有连接本身判得了;
    /// * 压根不是基表;落在 `LEFT JOIN` 的可空侧而 `allow_nullable` 又是假.
    ///
    /// `allow_nullable` 由 `opt` 按 **null-rejecting** 判定:可空侧会被外连接补出
    /// NULL,于是只有"遇到 NULL 必不成立"的谓词(`b.y = 5`,`b.y IS NOT NULL`)
    /// 才敢推--`b.y IS NULL` 那种不行(补出来的 NULL 也满足它,推下去那些行就没了,
    /// 见 [`Range::nullable`]).
    pub(crate) fn pushable_range(
        &self,
        cols: &BTreeSet<usize>,
        allow_nullable: bool,
    ) -> Option<usize> {
        let first = *cols.iter().next()?;
        let ri = self.ranges.iter().position(|range| range.contains(first))?;
        let range = &self.ranges[ri];

        if range.scan.is_none() || (range.nullable && !allow_nullable) {
            return None;
        }
        if !cols.iter().all(|&index| range.contains(index)) {
            return None;
        }
        Some(ri)
    }

    /// 符号表(`opt` 要拿它查目录里有没有索引).
    pub(crate) fn symbols(&self) -> &'s SymbolTable {
        self.symbols
    }

    /// 扁平 schema.
    fn schema(&self) -> Vec<Column> {
        self.ranges
            .iter()
            .flat_map(|r| r.columns.iter().cloned())
            .collect()
    }

    /// 扁平下标 → 那一列的类型.上下文定型要用.
    pub(crate) fn column_type_at(&self, index: usize) -> Option<DataType> {
        for range in &self.ranges {
            if index >= range.offset && index < range.offset + range.columns.len() {
                return Some(range.columns[index - range.offset].ty);
            }
        }
        None
    }

    fn column_names(&self) -> Vec<String> {
        self.ranges
            .iter()
            .flat_map(|r| r.columns.iter().map(|c| c.name.clone()))
            .collect()
    }

    /// 解一个列引用,返回**扁平下标**.
    pub(crate) fn resolve(&self, parts: &[String]) -> Result<Resolved> {
        // 先本层:SQL 的作用域规则就是"就近"
        if let Some(index) = self.local_index(parts)? {
            return Ok(Resolved::Local(index));
        }

        // 再一层层往外找(相关子查询就是从这里来的)
        let mut current = self;
        let mut depth = 1u16;
        while let Some(outer) = current.outers.first() {
            if let Some(index) = outer.local_index(parts)? {
                return Ok(Resolved::Outer { depth, index });
            }
            current = outer;
            depth += 1;
        }

        Err(DbError::UnknownColumn {
            name: parts.join("."),
            available: self.column_names(),
        })
    }

    /// 在**本层**解一个列引用;找不到给 `None`(好让调用方往外走).
    fn local_index(&self, parts: &[String]) -> Result<Option<usize>> {
        let (qualifier, name) = match parts {
            [] => {
                return Err(DbError::UnknownColumn {
                    name: String::new(),
                    available: self.column_names(),
                })
            }
            [name] => (None, name.as_str()),
            [.., qualifier, name] => (Some(qualifier.as_str()), name.as_str()),
        };

        let mut hits: Vec<(usize, usize)> = Vec::new();
        for (ri, range) in self.ranges.iter().enumerate() {
            if let Some(q) = qualifier {
                if !range.matches(q) {
                    continue;
                }
            }
            for (ci, column) in range.columns.iter().enumerate() {
                if column.name == name {
                    hits.push((ri, ci));
                }
            }
        }

        match hits.len() {
            0 => Ok(None),
            1 => {
                let (ri, ci) = hits[0];
                Ok(Some(self.ranges[ri].offset + ci))
            }
            _ => Err(DbError::AmbiguousColumn {
                name: name.to_string(),
                candidates: hits
                    .iter()
                    .map(|(ri, _)| format!("{}.{}", self.ranges[*ri].qualified(), name))
                    .collect(),
            }),
        }
    }

    /// 外层某一层的第 `index` 列的类型.
    fn outer_type_at(&self, depth: u16, index: usize) -> Option<DataType> {
        let mut current = self;
        for _ in 1..depth {
            current = current.outers.first()?;
        }
        current.outers.first()?.column_type_at(index)
    }
}
