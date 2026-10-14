//! AST → 线性 IR 的降级（绑定器）。
//!
//! 输入是 `pg_query`（PostgreSQL 服务端的真实解析器）产出的**裸语法树**，输出是
//! [`Plan`]。中间要做的正是 PostgreSQL 里 `transformStmt` 那一步的缩小版：
//!
//! * **查表**：`FROM` 里的每个名字去 [`SymbolTable`] 换出列定义；
//! * **解列名**：`x`、`f.x`、`study.people.x` 都要落到**扁平下标**上；
//! * **类型检查**：交给下游 [`Plan::analyze`] 与 [`Prog::verify`]；
//! * **确认形状**：`AS` 只影响限定名匹配，不改变列名（列名保持表自己的）。
//!
//! # 一条铁律：不认识的写法一律拒绝
//!
//! 凡是指向**尚未实现**能力的写法，一律**显式报 [`DbError::Unsupported`]**，
//! 绝不猜。宁可拒绝，也不要给一个安静的错误答案——`IS NULL` 就是个例子：在值
//! 模型还没有 NULL 的年代，若把它当"恒为假"执行，`died IS NULL` 会安静地返回
//! 0 行，**错得很隐蔽**，比报错危险得多。
//!
//! 仍未实现的（都在本文件里显式拒绝）：子查询（标量 / `IN` / `EXISTS` / 派生表 /
//! `ALL`）、集合运算、CTE、`DISTINCT`、窗口函数、`GROUPING SETS`/`ROLLUP`/`CUBE`、
//! `RIGHT`/`FULL`/`SEMI`/`ANTI` JOIN、`JOIN ... USING` 与 `NATURAL JOIN`、
//! `UPDATE`/`DELETE`/`DROP`/`ALTER`、DDL 的各种变体（`CREATE INDEX`/`VIEW`/
//! `AS SELECT`…）、事务、参数占位符、数组与位串类型。
//!
//! # 绑定期的作用域
//!
//! 列引用解出来的是**相对算子输入 schema 的扁平下标**。`FROM a, b` 或
//! `a JOIN b` 之后，schema 是「左 ++ 右」，所以 `Scope` 记着每一项的偏移量，
//! 解列名时先定位到项、再加上偏移。这个扁平 schema 必须与
//! [`Plan::analyze`] 推导出来的完全一致——若哪天不一致，`analyze` 会以
//! `IrInvalid` 报出来，而不会静默读错列。

use pg_query::protobuf::{
    a_const::Val, node, AConst, AExpr, AExprKind, BoolExpr, BoolExprType, CaseExpr, ColumnDef,
    ConstrType, CreateStmt, FuncCall, InsertStmt, JoinExpr, JoinType, LimitOption, Node,
    NullTestType, OnCommitAction, RangeVar, SelectStmt, SetOperation, SortBy, SortByDir, SortByNulls,
    TypeName,
};

use crate::db::table::{
    Column, DataType, DbError, Result, TypeFamily, Value,
    DEFAULT_TIMESTAMP_PRECISION,
    MAX_TIMESTAMP_PRECISION,
};

use super::expr::{BinOp, Prog, ProgBuilder, Reg, ScalarFunc, ScalarType, UnOp};
use super::plan::{
    passthrough_column, project_column, AggFunc, AggSpec, JoinKind, NodeId, Op, Plan, PlanBuilder,
    SortKey,
};
use super::stmt::Stmt;
use super::symbol::SymbolTable;

/// 解析 SQL 并降级成**恰好一条查询**。
///
/// 这是给 `EXPLAIN` / [`crate::ir::compile`] 用的：它只要查询。要跑脚本（可能
/// 含多条语句、可能含 DDL/DML）用 [`crate::ir::run_script`]。
pub fn lower(sql: &str, symbols: &SymbolTable) -> Result<Plan> {
    let parsed = pg_query::parse(sql).map_err(|e| DbError::Parse {
        message: e.to_string(),
    })?;

    if parsed.protobuf.stmts.is_empty() {
        return Err(DbError::Parse {
            message: "没有解析出任何语句".to_string(),
        });
    }
    if parsed.protobuf.stmts.len() > 1 {
        return Err(unsupported(format!(
            "这里只接受一条查询（收到 {} 条语句）",
            parsed.protobuf.stmts.len()
        )));
    }

    let node = parsed.protobuf.stmts[0]
        .stmt
        .as_deref()
        .ok_or_else(|| DbError::Parse {
            message: "空的语句".to_string(),
        })?;

    match lower_statement(node, symbols)? {
        Stmt::Query(plan) => Ok(plan),
        _ => Err(unsupported(
            "这不是一条查询语句（EXPLAIN / compile 只接受 SELECT；建表与插数据请直接执行）",
        )),
    }
}

/// 把一个已经解析好的语句节点降级成 [`Stmt`]。
pub fn lower_statement(stmt: &Node, symbols: &SymbolTable) -> Result<Stmt> {
    match &stmt.node {
        Some(node::Node::SelectStmt(select)) => Ok(Stmt::Query(lower_select(select, symbols)?)),
        Some(node::Node::CreateStmt(create)) => lower_create_table(create, symbols),
        Some(node::Node::InsertStmt(insert)) => lower_insert(insert, symbols),
        Some(node::Node::UpdateStmt(_)) => Err(unsupported("UPDATE")),
        Some(node::Node::DeleteStmt(_)) => Err(unsupported("DELETE")),
        Some(node::Node::DropStmt(_)) => Err(unsupported("DROP")),
        Some(node::Node::AlterTableStmt(_)) => Err(unsupported("ALTER TABLE")),
        Some(node::Node::CreateTableAsStmt(_)) => Err(unsupported("CREATE TABLE ... AS SELECT")),
        Some(node::Node::IndexStmt(_)) => Err(unsupported("CREATE INDEX")),
        Some(node::Node::ViewStmt(_)) => Err(unsupported("CREATE VIEW")),
        Some(node::Node::TransactionStmt(_)) => Err(unsupported("事务（BEGIN / COMMIT / ROLLBACK）")),
        _ => Err(unsupported("这种语句")),
    }
}

// ------------------------------------------------------------- CREATE TABLE

fn lower_create_table(create: &CreateStmt, _symbols: &SymbolTable) -> Result<Stmt> {
    // 本版只认最朴素的 `CREATE TABLE name (列定义, ...)`
    if !create.inh_relations.is_empty() {
        return Err(unsupported("INHERITS"));
    }
    if create.partbound.is_some() || create.partspec.is_some() {
        return Err(unsupported("PARTITION BY / PARTITION OF"));
    }
    if create.of_typename.is_some() {
        return Err(unsupported("CREATE TABLE ... OF <类型>"));
    }
    if !create.access_method.is_empty() {
        return Err(unsupported("USING <访问方法>"));
    }
    if !create.tablespacename.is_empty() {
        return Err(unsupported("TABLESPACE"));
    }
    if !create.options.is_empty() {
        return Err(unsupported("WITH (...) 存储参数"));
    }
    // 注意：`OnCommitAction::OncommitNoop = 1`，**不是** 0。没写 ON COMMIT 时
    // 这个字段就是 1，所以不能拿 `!= 0` 当"没指定"。（和 `SetopNone = 1` 同一
    // 类坑：枚举里表示"无"的那个变体不是 0。）
    let on_commit = OnCommitAction::try_from(create.oncommit).unwrap_or(OnCommitAction::Undefined);
    if on_commit != OnCommitAction::OncommitNoop {
        return Err(unsupported("ON COMMIT"));
    }
    if !create.constraints.is_empty() {
        // 表级约束（`CREATE TABLE t (a INT, PRIMARY KEY (a))`）本版不做。
        // 注意列级约束不在这里，它们在每个 ColumnDef.constraints 里。
        return Err(unsupported(
            "表级约束（例如 PRIMARY KEY (a) 写在列定义之外）——请写成列级约束",
        ));
    }

    let relation = create
        .relation
        .as_ref()
        .ok_or_else(|| unsupported("没有表名"))?;
    let name = qualified_name(relation)?;

    let mut columns = Vec::with_capacity(create.table_elts.len());
    let mut unenforced = Vec::new();

    for elt in &create.table_elts {
        let def = match &elt.node {
            Some(node::Node::ColumnDef(def)) => def,
            // 实测：`CREATE TABLE p (a INT, PRIMARY KEY (a))` 里那条 PRIMARY KEY
            // 会作为 Constraint 出现在 **table_elts** 里，而不是 CreateStmt.constraints。
            // 这两种写法都拒绝，消息要指得准。
            Some(node::Node::Constraint(_)) => {
                return Err(unsupported(
                    "表级约束（例如 PRIMARY KEY (a) 写在列定义之外）——请写成列级约束",
                ))
            }
            _ => {
                return Err(unsupported(
                    "表元素里的这种项（本版只支持列定义与列级约束）",
                ))
            }
        };
        if def.raw_default.is_some() || def.cooked_default.is_some() {
            return Err(unsupported("列默认值"));
        }
        if !def.identity.is_empty() || def.identity_sequence.is_some() {
            return Err(unsupported("IDENTITY 列"));
        }
        if !def.generated.is_empty() {
            return Err(unsupported("生成列（GENERATED ... AS）"));
        }
        if def.coll_clause.is_some() {
            return Err(unsupported("COLLATE"));
        }
        let not_null = if !def.constraints.is_empty() {
            collect_constraints(def, &mut unenforced)?
        } else {
            false
        };

        let ty = column_type(
            def.type_name
                .as_ref()
                .ok_or_else(|| unsupported("列没有类型"))?,
        )?;
        columns.push(if not_null {
            Column::not_null(def.colname.clone(), ty)
        } else {
            Column::new(def.colname.clone(), ty)
        });
    }

    if columns.is_empty() {
        return Err(unsupported("没有任何列的表"));
    }

    Ok(Stmt::CreateTable {
        name,
        columns,
        if_not_exists: create.if_not_exists,
        unenforced,
    })
}

/// 逐条列级约束分类。见模块文档里那张表：**能真成立的就接受，有行为语义或
/// 会静默违规的就拒绝**。
/// 返回这一列是不是 `NOT NULL`。
fn collect_constraints(def: &ColumnDef, unenforced: &mut Vec<String>) -> Result<bool> {
    let mut not_null = false;
    for constraint in &def.constraints {
        let c = match &constraint.node {
            Some(node::Node::Constraint(c)) => c,
            _ => return Err(unsupported("列约束里的这种项")),
        };

        match ConstrType::try_from(c.contype).unwrap_or(ConstrType::Undefined) {
            // **真的强制**：`Column::not_null` 会让 `Table::validate` 拦住 NULL。
            // 所以它既不属于"没强制"，也不需要额外提示。
            ConstrType::ConstrNotnull => not_null = true,

            // 接受，但本版不做唯一性检查，必须让它看得见。
            ConstrType::ConstrPrimary => {
                unenforced.push(format!("PRIMARY KEY ({})", def.colname));
            }
            ConstrType::ConstrUnique => {
                unenforced.push(format!("UNIQUE ({})", def.colname));
            }

            // 有行为语义或会静默违规的，一律拒绝。
            ConstrType::ConstrDefault => return Err(unsupported("DEFAULT（有行为语义，忽略会让 INSERT 行为悄悄变样）")),
            ConstrType::ConstrCheck => return Err(unsupported("CHECK（本版不强制，会静默违规）")),
            ConstrType::ConstrForeign => return Err(unsupported("REFERENCES（外键）")),
            ConstrType::ConstrGenerated => return Err(unsupported("GENERATED")),
            ConstrType::ConstrIdentity => return Err(unsupported("IDENTITY")),
            ConstrType::ConstrNull => {}

            other => {
                return Err(unsupported(format!("列约束 {:?}", other)));
            }
        }
    }
    Ok(not_null)
}

/// `TypeName` → 本版的 [`DataType`]。
///
/// 关键：`INT` 在语法树里是 `names = ["pg_catalog", "int4"]`——**内部的、带
/// schema 前缀的**名字。所以取**最后一段**再认。
fn column_type(type_name: &TypeName) -> Result<DataType> {
    if !type_name.array_bounds.is_empty() {
        return Err(unsupported("数组类型"));
    }
    let last = type_name
        .names
        .last()
        .and_then(|n| match &n.node {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .ok_or_else(|| unsupported("看不懂的类型名"))?;

    // typmods 是**裸字面量**：`VARCHAR(30)` 给 `[Ival(30)]`，`NUMERIC(10,2)` 给
    // `[Ival(10), Ival(2)]`。没有 PostgreSQL 内部那种 N+4 偏移。
    let mods = type_mods(type_name)?;
    let need = |what: &str, index: usize| -> Result<u32> {
        mods.get(index).copied().ok_or_else(|| {
            unsupported(format!(
                "`{}` 需要写全参数（例如 {}(N)），裸的写法本版不接受",
                what, what
            ))
        })
    };

    match last.as_str() {
        "int4" | "int" | "integer" => Ok(DataType::Int),
        "text" => Ok(DataType::Text),
        // 语法树里 `CHAR` 的实名是 `bpchar`
        "bpchar" | "char" => Ok(DataType::Char(match mods.first() {
            // SQL 标准：裸 `CHAR` 就是 `CHAR(1)`
            None => 1,
            Some(n) => *n,
        })),
        "varchar" => Ok(DataType::VarChar(need("VARCHAR", 0)?)),
        "timestamp" | "timestamptz" => Ok(DataType::Timestamp(match mods.first() {
            None => DEFAULT_TIMESTAMP_PRECISION,
            Some(n) if *n <= MAX_TIMESTAMP_PRECISION => *n,
            Some(n) => {
                return Err(unsupported(format!(
                    "TIMESTAMP({})：小数秒上限是 {}",
                    n, MAX_TIMESTAMP_PRECISION
                )))
            }
        })),
        "numeric" | "decimal" => match (mods.first(), mods.get(1)) {
            // `NUMERIC(N, M)`
            (Some(p), Some(s)) => Ok(DataType::Numeric(*p, *s)),
            // `NUMERIC(N)`：与 PostgreSQL 一致，等于标度 0
            (Some(p), None) => Ok(DataType::Numeric(*p, 0)),
            // 裸 `NUMERIC`：精度与标度都不限
            _ => Ok(DataType::NumericAny),
        },
        other => Err(unsupported(format!("类型 `{}`", other))),
    }
}

/// 取出 `TypeName.typmods` 里的整数字面量。
fn type_mods(type_name: &TypeName) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(type_name.typmods.len());
    for m in &type_name.typmods {
        match &m.node {
            Some(node::Node::AConst(c)) => match &c.val {
                Some(Val::Ival(i)) if i.ival >= 0 => out.push(i.ival as u32),
                _ => return Err(unsupported("类型参数不是非负整数")),
            },
            _ => return Err(unsupported("类型参数不是常量")),
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
        return Err(unsupported("WITH（公共表表达式）"));
    }

    let relation = insert
        .relation
        .as_ref()
        .ok_or_else(|| unsupported("没有表名"))?;
    let table = qualified_name(relation)?;

    let target = symbols
        .columns(&table)
        .ok_or_else(|| DbError::UnknownSymbol { name: table.clone() })?
        .to_vec();

    // 行数据在 select_stmt 里：`VALUES (...), (...)` 会变成带 values_lists 的
    // SelectStmt，而 `INSERT ... SELECT` 的 values_lists 是空的。
    let select = match insert.select_stmt.as_deref().and_then(|s| s.node.as_ref()) {
        Some(node::Node::SelectStmt(select)) => select,
        _ => return Err(unsupported("INSERT 后面的这种来源（本版只支持 VALUES）")),
    };
    if select.values_lists.is_empty() {
        if select.target_list.is_empty() {
            return Err(unsupported("INSERT ... DEFAULT VALUES"));
        }
        return Err(unsupported("INSERT ... SELECT"));
    }

    // 列名表 → 「表的第 i 列的值在行内第几个位置」
    let permutation = column_permutation(insert, &target, &table)?;

    let mut rows = Vec::with_capacity(select.values_lists.len());
    for values in &select.values_lists {
        let items = match &values.node {
            Some(node::Node::List(list)) => &list.items,
            _ => return Err(unsupported("VALUES 里的这种项")),
        };
        if items.len() != target.len() {
            return Err(unsupported(format!(
                "每一行要给 {} 个值，实际给了 {} 个",
                target.len(),
                items.len()
            )));
        }

        // 按表列序重排。值表达式没有输入行，所以作用域是空的——
        // 一旦里面出现列引用，Prog::verify 就会以「列下标越界」把它挡下来。
        let scope = Scope::default();
        let mut row = Vec::with_capacity(items.len());
        for (index, &source) in permutation.iter().enumerate() {
            let mut p = ProgBuilder::new();
            // 用目标列的类型给这个值定型（`'Alice'` → TEXT、`'2024-01-15'` →
            // TIMESTAMP、`1.5` → NUMERIC）。不做这一步，新类型基本插不进数据。
            let reg = lower_expr_typed(
                &items[source],
                &scope,
                Some(&target[index].ty),
                &mut p,
            )?;
            row.push(p.finish(reg));
        }
        rows.push(row);
    }

    Ok(Stmt::Insert { table, rows })
}

/// 算出「表的第 i 列，值应该从行内第几个位置取」。
fn column_permutation(insert: &InsertStmt, target: &[Column], table: &str) -> Result<Vec<usize>> {
    use std::collections::{BTreeMap, BTreeSet};

    // 没写列名表：值就按表的列序
    if insert.cols.is_empty() {
        return Ok((0..target.len()).collect());
    }

    let mut wanted: BTreeMap<String, usize> = BTreeMap::new();
    for (position, col) in insert.cols.iter().enumerate() {
        let name = match &col.node {
            Some(node::Node::ResTarget(rt)) => rt.name.clone(),
            _ => return Err(unsupported("列名表里的这种项")),
        };
        if wanted.insert(name.clone(), position).is_some() {
            return Err(unsupported(format!("列 `{}` 在列名表里出现了两次", name)));
        }
    }

    // **先**查"表上根本没有这一列"。顺序很重要：若先按表列找，用户把列名写错时
    // 会先撞上"某一列没被覆盖"，于是收到"你省略了那一列"——完全误导。实测就是
    // 这样，被测试逮住了。
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

    // 到这里 wanted 里的名字都是真列名，只需再查有没有漏列。
    let mut permutation = Vec::with_capacity(target.len());
    for column in target {
        match wanted.get(&column.name) {
            Some(&position) => permutation.push(position),
            None => {
                return Err(unsupported(format!(
                    "INSERT 省略了 `{}.{}` 的值（本版没有列默认值，必须每列都给）",
                    table, column.name
                )))
            }
        }
    }

    Ok(permutation)
}

// ------------------------------------------------------------------ SELECT

fn lower_select(select: &SelectStmt, symbols: &SymbolTable) -> Result<Plan> {
    // ---- 守卫：本版表达不了的 SELECT 形态，一律显式拒绝
    let set_op = SetOperation::try_from(select.op).unwrap_or(SetOperation::Undefined);
    if set_op != SetOperation::SetopNone {
        return Err(unsupported(match set_op {
            SetOperation::SetopUnion => "集合运算 UNION",
            SetOperation::SetopIntersect => "集合运算 INTERSECT",
            SetOperation::SetopExcept => "集合运算 EXCEPT",
            _ => "集合运算",
        }));
    }
    if select.with_clause.is_some() {
        return Err(unsupported("WITH（公共表表达式）"));
    }
    if !select.distinct_clause.is_empty() {
        return Err(unsupported("DISTINCT"));
    }
    if !select.window_clause.is_empty() {
        return Err(unsupported("窗口函数（OVER）"));
    }
    if !select.values_lists.is_empty() {
        return Err(unsupported("VALUES 列表"));
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
            message: "SELECT 没有任何输出列".to_string(),
        });
    }

    let mut b = PlanBuilder::new();

    // ---- FROM
    let (from_id, scope) = lower_from(&select.from_clause, &mut b, symbols)?;
    let schema = scope.schema();

    // ---- WHERE（Filter 不改变 schema，所以后面几个都还按同一个 scope 解）
    let filtered = match &select.where_clause {
        Some(expr) => {
            let mut p = ProgBuilder::new();
            let reg = lower_expr(expr, &scope, &mut p)?;
            b.filter(from_id, p.finish(reg))?
        }
        None => from_id,
    };

    // ---- 是分组查询吗
    //
    // 三种情况都算：写了 GROUP BY；写了 HAVING；输出列表或 ORDER BY 里出现了聚合。
    // 最后一种正是 `SELECT COUNT(*) FROM t`（整表一组）。
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
    // SQL 里 `ORDER BY` 既能用输出列的别名，也能用输入列，两者解析规则不同。
    // 做法：先按**输出 schema** 解，解得动就把 Sort 放在 Project 之上；
    // 解不动再按**输入 schema** 解，此时 Sort 放在 Project 之下。
    // 两边都解不动，就报按输出 schema 解出来的那个错（对用户更有意义）。
    let output_columns: Vec<Column> = progs
        .iter()
        .zip(names.iter())
        .map(|(prog, name)| project_column(prog, name, &schema))
        .collect::<Result<_>>()?;
    let output_scope = Scope::from_range(String::new(), None, output_columns);

    let (sort_keys, sort_below_project) = if select.sort_clause.is_empty() {
        (Vec::new(), false)
    } else {
        match lower_sort_keys(&select.sort_clause, &output_scope) {
            Ok(keys) => (keys, false),
            Err(above) => match lower_sort_keys(&select.sort_clause, &scope) {
                Ok(keys) => (keys, true),
                Err(_) => return Err(above),
            },
        }
    };

    // ---- 拼装：Sort(下) → Project → Sort(上) → Limit
    let mut current = filtered;

    // 没有 ORDER BY 时 sort_keys 为空，**不生成 Sort 节点**
    // （空排序键是非法计划，analyze 会拒绝）。
    if sort_below_project && !sort_keys.is_empty() {
        current = b.sort(current, sort_keys.clone())?;
    }

    if !identity {
        current = b.project(current, progs, names.clone())?;
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

/// 分组查询的管道：
///
/// ```text
/// Limit( Sort( Project( Filter(HAVING)( Aggregate( 输入 ) ) ) ) )
/// ```
///
/// 顺序上有一处**不能动**：`SELECT` 列表与 `HAVING` 都要在**建 Aggregate 节点之前**
/// 降级完，因为 `HAVING` 里可能第一次出现某个聚合，后建节点才能把它的规格收全。
fn lower_grouped(
    select: &SelectStmt,
    input: NodeId,
    scope: &Scope,
    input_schema: &[Column],
    mut b: PlanBuilder,
) -> Result<Plan> {
    let mut agg = AggLowering::default();

    // ---- GROUP BY 的分组键（按输入行求值）
    for group in &select.group_clause {
        if matches!(group.node, Some(node::Node::GroupingSet(_))) {
            return Err(unsupported("GROUPING SETS / ROLLUP / CUBE"));
        }
        let mut p = ProgBuilder::new();
        let reg = lower_expr(group, scope, &mut p)?;
        agg.groups.push(p.finish(reg));
        agg.group_names.push(derived_name(group));
    }

    // ---- SELECT 列表与 HAVING（外层），都先降级，把聚合收齐
    let (progs, names) = lower_target_list_agg(&select.target_list, scope, &mut agg)?;

    let having = match &select.having_clause {
        Some(expr) => {
            let mut p = ProgBuilder::new();
            let reg = lower_expr_agg(expr, scope, &mut agg, &mut p)?;
            Some(p.finish(reg))
        }
        None => None,
    };

    // ---- ORDER BY 的排序键（也要在**建 Aggregate 之前**降级：排序键里可能
    //      第一次出现某个聚合，例如 `ORDER BY AVG(x)`）
    let mut sort_keys = Vec::with_capacity(select.sort_clause.len());
    for item in &select.sort_clause {
        let sb = sort_by(item)?;
        let (desc, nulls_first) = sort_direction(sb)?;

        let expr = sb
            .node
            .as_deref()
            .ok_or_else(|| unsupported("空的排序键"))?;
        // 别名先换成表达式，于是所有键都按分组降级、统一索引聚合输出
        let expr = substitute_alias(expr, &select.target_list);

        let mut p = ProgBuilder::new();
        let reg = lower_expr_agg(expr, scope, &mut agg, &mut p)?;
        sort_keys.push(SortKey {
            prog: p.finish(reg),
            desc,
            nulls_first,
        });
    }

    // ---- 现在聚合规格齐了，建 Aggregate
    let agg_names: Vec<String> = agg
        .specs
        .iter()
        .map(|spec| spec.func.name().to_lowercase())
        .collect();
    // 这里只为数量对得上做一次校验；`analyze` 才是权威
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

    // ---- HAVING：在 Aggregate 之上、Project 之下（所以用不了输出别名，与 PG 一致）
    if let Some(having) = having {
        current = b.filter(current, having)?;
    }

    // ---- ORDER BY：排序键索引的是**聚合输出**，所以 Sort 必须在 Project 之下
    if !sort_keys.is_empty() {
        current = b.sort(current, sort_keys)?;
    }

    // ---- 投影
    current = b.project(current, progs, names.clone())?;

    let (limit, offset) = lower_limit(select)?;
    if limit.is_some() || offset > 0 {
        current = b.limit(current, limit, offset)?;
    }

    b.finish(current)
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
                .map_err(|_| unsupported(format!("{} 必须是常量整数", what))),
            _ => Err(unsupported(format!("{} 必须是常量整数", what))),
        },
        _ => Err(unsupported(format!("{} 必须是常量整数", what))),
    }
}

// -------------------------------------------------------------------- FROM

fn lower_from(
    from: &[Node],
    b: &mut PlanBuilder,
    symbols: &SymbolTable,
) -> Result<(NodeId, Scope)> {
    // 没有 FROM：`SELECT 1 + 1` 这种，底座是单行零列。
    if from.is_empty() {
        return Ok((b.single_row(), Scope::default()));
    }

    let mut iter = from.iter();
    let first = iter.next().expect("from 非空");
    let (mut id, mut scope) = lower_from_item(first, b, symbols)?;

    // `FROM a, b`：pg_query 给的是两个元素、**没有 JoinExpr**。
    // 左深折叠成无条件内连接（笛卡尔积），过滤条件由 WHERE 提供。
    for item in iter {
        let (right_id, right_scope) = lower_from_item(item, b, symbols)?;
        id = b.join(id, right_id, JoinKind::Inner, None)?;
        scope = Scope::concat(scope, right_scope);
    }

    Ok((id, scope))
}

fn lower_from_item(
    node: &Node,
    b: &mut PlanBuilder,
    symbols: &SymbolTable,
) -> Result<(NodeId, Scope)> {
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
                    return Err(unsupported("AS 后面带列名表（AS t(a, b)）"));
                }
            }

            let id = b.scan(&name);
            // 别名只用于限定名匹配（`t.x`），**不改列名**——列名保持表自己的，
            // 这样 Scope 的 schema 才与 `Plan::analyze` 推出来的一致。
            Ok((id, Scope::from_range(name, alias, columns)))
        }

        Some(node::Node::JoinExpr(j)) => lower_join(j, b, symbols),

        Some(node::Node::RangeSubselect(_)) => {
            Err(unsupported("FROM 里的子查询（派生表）"))
        }
        Some(node::Node::RangeFunction(_)) => Err(unsupported("FROM 里的函数调用")),
        _ => Err(unsupported("FROM 里的这种项")),
    }
}

fn lower_join(
    j: &JoinExpr,
    b: &mut PlanBuilder,
    symbols: &SymbolTable,
) -> Result<(NodeId, Scope)> {
    if j.is_natural {
        return Err(unsupported("NATURAL JOIN"));
    }
    if !j.using_clause.is_empty() {
        // USING 会把连接列**并成一列**，本版的输出 schema 表达不了这个合并，
        // 所以宁可拒绝，也不给一个列重复出现的错误结果。
        return Err(unsupported(
            "JOIN ... USING（它会把连接列并成一列，本版的扁平 schema 表达不了；请改用 ON）",
        ));
    }
    if j.join_using_alias.is_some() {
        return Err(unsupported("USING 后面的别名"));
    }

    let join_kind = match JoinType::try_from(j.jointype).unwrap_or(JoinType::Undefined) {
        JoinType::JoinInner => JoinKind::Inner,
        JoinType::JoinLeft => JoinKind::Left,
        other => {
            return Err(unsupported(match other {
                JoinType::JoinRight => "RIGHT JOIN",
                JoinType::JoinFull => "FULL JOIN",
                JoinType::JoinSemi => "SEMI JOIN",
                JoinType::JoinAnti => "ANTI JOIN",
                _ => "这种连接类型",
            }))
        }
    };

    let larg = j.larg.as_deref().ok_or_else(|| unsupported("连接缺左支"))?;
    let rarg = j.rarg.as_deref().ok_or_else(|| unsupported("连接缺右支"))?;

    let (left_id, left_scope) = lower_from_item(larg, b, symbols)?;
    let (right_id, right_scope) = lower_from_item(rarg, b, symbols)?;
    let scope = Scope::concat(left_scope, right_scope);

    let on = match &j.quals {
        Some(expr) => {
            let mut p = ProgBuilder::new();
            let reg = lower_expr(expr, &scope, &mut p)?;
            Some(p.finish(reg))
        }
        None => None,
    };

    let id = b.join(left_id, right_id, join_kind, on)?;
    Ok((id, scope))
}

fn qualified_name(rv: &RangeVar) -> Result<String> {
    if !rv.catalogname.is_empty() {
        return Err(unsupported(format!(
            "跨库引用 `{}.{}`",
            rv.catalogname, rv.relname
        )));
    }
    if rv.schemaname.is_empty() {
        Ok(rv.relname.clone())
    } else {
        Ok(format!("{}.{}", rv.schemaname, rv.relname))
    }
}

// --------------------------------------------------------------- 输出列表

fn lower_target_list(targets: &[Node], scope: &Scope) -> Result<(Vec<Prog>, Vec<String>)> {
    let mut progs = Vec::new();
    let mut names = Vec::new();

    for target in targets {
        let rt = match &target.node {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => return Err(unsupported("SELECT 列表里的这种元素")),
        };
        let value = rt
            .val
            .as_deref()
            .ok_or_else(|| unsupported("空的输出列"))?;

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

/// 没有 `AS` 时，列名的推导规则：列引用用列名，其它用 `?column?`（跟 PostgreSQL 一致）。
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

/// 这个程序是不是「原样取第 i 列」。用来省掉恒等的 `Project`。
fn is_identity(progs: &[Prog], names: &[String], schema: &[Column]) -> bool {
    progs.len() == schema.len()
        && progs
            .iter()
            .enumerate()
            .all(|(i, p)| passthrough_column(p) == Some(i))
        && names.iter().zip(schema.iter()).all(|(n, c)| *n == c.name)
}

// ---------------------------------------------------------------- ORDER BY

fn lower_sort_keys(sort_clause: &[Node], scope: &Scope) -> Result<Vec<SortKey>> {
    let mut keys = Vec::with_capacity(sort_clause.len());

    for item in sort_clause {
        let sb = sort_by(item)?;
        let (desc, nulls_first) = sort_direction(sb)?;

        let expr = sb
            .node
            .as_deref()
            .ok_or_else(|| unsupported("空的排序键"))?;

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

fn sort_by(item: &Node) -> Result<&SortBy> {
    match &item.node {
        Some(node::Node::SortBy(sb)) => Ok(sb),
        _ => Err(unsupported("ORDER BY 里的这种项")),
    }
}

/// `(是否降序, NULL 是否排最前)`。
///
/// NULL 的位置是**绝对**的，PostgreSQL 的默认是 ASC → NULLS LAST、
/// DESC → NULLS FIRST。
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

/// `ORDER BY` 里的裸标识符如果是**显式输出别名**，就换成它对应的表达式。
///
/// 这一步必须在降级**之前**做：换完以后所有排序键都按「分组」降级，于是它们统一
/// 索引 `Aggregate` 的输出 schema，`Sort` 就能干净地放在 `Project` 之下。
/// 不然一个子句里混着别名（索引输出）与分组列（索引聚合输出），一个 `Sort`
/// 节点没法同时满足两种下标。
///
/// 注意 `ResTarget.name` **只有写了 `AS` 才有值**，所以这里只会命中真别名，
/// 不会把 `ORDER BY s.student_id` 误当成别名。
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

/// 降级一个表达式，不允许用上下文定型字面量。
fn lower_expr(node: &Node, scope: &Scope, p: &mut ProgBuilder) -> Result<Reg> {
    lower_expr_in(node, scope, None, None, p)
}

/// 分组查询的外层：表达式按「每个分组算一次」降级。
fn lower_expr_agg(node: &Node, scope: &Scope, agg: &mut AggLowering, p: &mut ProgBuilder) -> Result<Reg> {
    lower_expr_in(node, scope, None, Some(agg), p)
}

/// 降级一个表达式，并用 `expected` 解释**无类型字面量**。
///
/// 为什么需要这个：语法树里的 `'2024-01-15'` 只是个字符串，而 `WHERE ts = '...'`
/// 里的 `ts` 是 TIMESTAMP。没有上下文定型，新类型根本没法用。
fn lower_expr_typed(
    node: &Node,
    scope: &Scope,
    expected: Option<&DataType>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    lower_expr_in(node, scope, expected, None, p)
}

/// 真正的实现。`agg` 为 `Some` 时是**分组查询的外层**：
/// 聚合调用变成对聚合输出的列引用、裸列引用去 `GROUP BY` 列表里找。
fn lower_expr_in(
    node: &Node,
    scope: &Scope,
    expected: Option<&DataType>,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    // 1) 聚合调用
    if let Some(node::Node::FuncCall(call)) = &node.node {
        if let Some(func) = aggregate_func(call) {
            let agg = agg
                .as_deref_mut()
                .ok_or_else(|| unsupported("聚合函数出现在了不能出现的位置（只能在 SELECT、HAVING、ORDER BY 里）"))?;
            let slot = agg.slot_for(node, call, func, scope)?;
            // 聚合输出 schema = [分组键...] ++ [聚合结果...]
            return Ok(p.col(agg.groups.len() + slot));
        }
    }

    // 2) 整个表达式就是某个分组键？
    if let Some(agg) = agg.as_deref() {
        if let Some(index) = agg.match_group(node, scope) {
            return Ok(p.col(index));
        }
    }

    match &node.node {
        Some(node::Node::ColumnRef(c)) => {
            let parts = ref_parts(&c.fields)
                .ok_or_else(|| unsupported("`*` 只能用在输出列表里，不能出现在表达式中"))?;
            let index = scope.resolve(&parts)?;
            // 分组模式下能走到这里，说明这一列既不是分组键、也没被聚合包住
            if agg.is_some() {
                return Err(unsupported(format!(
                    "分组查询里的列 `{}` 必须出现在 GROUP BY 里，或者被用在聚合函数里",
                    parts.join(".")
                )));
            }
            Ok(p.col(index))
        }

        Some(node::Node::AConst(c)) => lower_const(c, expected, p),

        Some(node::Node::TypeCast(tc)) => {
            // 只支持 `TYPE 'literal'` / `'literal'::TYPE` 这种「给字面量定型」，
            // 不支持任意的类型转换。
            let target = column_type(
                tc.type_name
                    .as_ref()
                    .ok_or_else(|| unsupported("没有目标类型的转换"))?,
            )?;
            match tc.arg.as_deref().and_then(|a| a.node.as_ref()) {
                Some(node::Node::AConst(c)) => lower_const(c, Some(&target), p),
                _ => Err(unsupported(
                    "只支持 `TYPE '字面量'` 形式的类型标注（不做任意类型转换）",
                )),
            }
        }

        Some(node::Node::AExpr(a)) => lower_aexpr(a, scope, agg.as_deref_mut(), p),

        Some(node::Node::BoolExpr(b)) => lower_boolexpr(b, scope, agg.as_deref_mut(), p),

        Some(node::Node::NullTest(nt)) => {
            if nt.argisrow {
                return Err(unsupported("行级的 IS NULL（IS NULL 用在 ROW 上）"));
            }
            let negated = match NullTestType::try_from(nt.nulltesttype)
                .unwrap_or(NullTestType::Undefined)
            {
                NullTestType::IsNull => false,
                NullTestType::IsNotNull => true,
                _ => return Err(unsupported("这种 NULL 测试")),
            };
            let arg = nt
                .arg
                .as_deref()
                .ok_or_else(|| unsupported("IS NULL 没有参数"))?;
            let src = lower_expr(arg, scope, p)?;
            Ok(p.is_null(src, negated))
        }
        Some(node::Node::FuncCall(call)) => {
            lower_func_call(call, scope, agg.as_deref_mut(), p)
        }
        Some(node::Node::SubLink(_)) => Err(unsupported("子查询")),
        Some(node::Node::CaseExpr(case)) => lower_case(case, scope, agg.as_deref_mut(), p),
        Some(node::Node::Param(_)) => Err(unsupported("参数占位符（$1）")),
        Some(node::Node::AStar(_)) => Err(unsupported("`*` 只能用在输出列表里")),
        _ => Err(unsupported("这种表达式")),
    }
}

/// `CASE WHEN ... THEN ... ELSE ... END`。
///
/// 从 ELSE 往前叠成嵌套的 [`Step::Case`](super::expr::Step::Case)。没有 `ELSE`
/// 且都不匹配时结果是 NULL——这与 SQL 一致。
fn lower_case(
    case: &CaseExpr,
    scope: &Scope,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    if case.arg.is_some() {
        return Err(unsupported(
            "简单 CASE（`CASE x WHEN ...`）——请写成 `CASE WHEN x = ... THEN ...` 形式",
        ));
    }

    let mut result = match &case.defresult {
        Some(default) => lower_expr_in(default, scope, None, agg.as_deref_mut(), p)?,
        None => p.value(Value::Null),
    };

    // 倒着叠：最内层是 ELSE，最外层是第一个 WHEN
    for when in case.args.iter().rev() {
        let arm = match &when.node {
            Some(node::Node::CaseWhen(arm)) => arm,
            _ => return Err(unsupported("CASE 里的这种项")),
        };
        let cond_node = arm
            .expr
            .as_deref()
            .ok_or_else(|| unsupported("CASE WHEN 没有条件"))?;
        let value_node = arm
            .result
            .as_deref()
            .ok_or_else(|| unsupported("CASE THEN 没有结果"))?;

        let cond = lower_expr_in(cond_node, scope, None, agg.as_deref_mut(), p)?;
        let value = lower_expr_in(value_node, scope, None, agg.as_deref_mut(), p)?;
        result = p.case(cond, value, result);
    }

    Ok(result)
}

/// 标量函数调用。目前只认 `ROUND`。
fn lower_func_call(
    call: &FuncCall,
    scope: &Scope,
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
        .ok_or_else(|| unsupported("看不懂的函数名"))?;

    // 聚合函数：只有当整个查询是分组查询时才会被 `lower_expr_agg` 接走；
    // 走到这里说明它出现在了不该出现的地方（例如 WHERE 里）。
    if aggregate_func(call).is_some() {
        if call.over.is_some() {
            return Err(unsupported("窗口函数（OVER）"));
        }
        return Err(unsupported(format!(
            "聚合函数 `{}` 出现在了不能出现的位置（只能在 SELECT、HAVING、ORDER BY 里）",
            name
        )));
    }
    if call.over.is_some() {
        return Err(unsupported("窗口函数（OVER）"));
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
                _ => return Err(unsupported("ROUND 的参数个数（要 1 个或 2 个）")),
            };
            Ok(p.call(ScalarFunc::Round, args))
        }
        other => Err(unsupported(format!("函数 `{}`", other))),
    }
}

/// 这是个聚合函数调用吗。窗口函数（带 `OVER`）**不算**——那是另一回事。
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

/// 这棵表达式子树里有没有聚合调用。
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
        Some(node::Node::NullTest(n)) => n.arg.as_deref().is_some_and(contains_aggregate),
        Some(node::Node::TypeCast(t)) => t.arg.as_deref().is_some_and(contains_aggregate),
        Some(node::Node::ResTarget(r)) => r.val.as_deref().is_some_and(contains_aggregate),
        _ => false,
    }
}

/// 分组查询降级时的状态。
///
/// **一趟到底**：外层的表达式边降级边登记聚合，而不是先走一遍收集再走一遍改写。
/// 两趟写法要求两次遍历的访问顺序完全一致，错了就会**静默串位**；用 AST 节点地址
/// 认同一个聚合，一趟就够，也不会重复登记同一个 `COUNT(*)`。
#[derive(Default)]
struct AggLowering {
    /// `GROUP BY` 的表达式，按输入行求值。
    groups: Vec<Prog>,
    /// 分组键的输出列名（仅用于展示／给聚合节点起名）。
    group_names: Vec<String>,
    /// 已登记的聚合。
    specs: Vec<AggSpec>,
    /// 哪个 AST 聚合调用节点对应哪个槽位（按节点地址认）。
    slots: Vec<(*const Node, usize)>,
}

impl AggLowering {
    /// 登记一个聚合，返回它的槽位。同一个 AST 节点只登记一次。
    fn slot_for(
        &mut self,
        node: &Node,
        call: &FuncCall,
        func: AggFunc,
        scope: &Scope,
    ) -> Result<usize> {
        let address = node as *const Node;
        if let Some((_, slot)) = self.slots.iter().find(|(n, _)| std::ptr::eq(*n, address)) {
            return Ok(*slot);
        }

        if call.agg_filter.is_some() {
            return Err(unsupported("聚合函数后的 FILTER 子句"));
        }
        if !call.agg_order.is_empty() {
            return Err(unsupported("聚合函数里的 ORDER BY"));
        }

        let arg = if call.agg_star {
            None
        } else {
            match call.args.as_slice() {
                [only] => {
                    let mut p = ProgBuilder::new();
                    let reg = lower_expr(only, scope, &mut p)?;
                    Some(p.finish(reg))
                }
                [] => return Err(unsupported(format!("{}() 需要参数", func.name()))),
                _ => {
                    return Err(unsupported(format!(
                        "{}(...) 只接受一个参数",
                        func.name()
                    )))
                }
            }
        };

        let slot = self.specs.len();
        self.specs.push(AggSpec {
            func,
            arg,
            distinct: call.agg_distinct,
        });
        self.slots.push((address, slot));
        Ok(slot)
    }

    /// 这个表达式整体是不是某个分组键；是就返回它的下标。
    fn match_group(&self, node: &Node, scope: &Scope) -> Option<usize> {
        if self.groups.is_empty() {
            return None;
        }
        // 按输入行降级一次再比程序。含聚合的表达式这里会失败，于是"不算分组键"。
        let mut p = ProgBuilder::new();
        let reg = lower_expr(node, scope, &mut p).ok()?;
        let prog = p.finish(reg);
        self.groups.iter().position(|group| *group == prog)
    }
}

/// 算 `Aggregate` 节点的输出 schema（与 `Op::Aggregate` 的 analyze 规则一致）。
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

/// `SELECT` 列表，按**分组**求值。
fn lower_target_list_agg(
    targets: &[Node],
    scope: &Scope,
    agg: &mut AggLowering,
) -> Result<(Vec<Prog>, Vec<String>)> {
    let mut progs = Vec::new();
    let mut names = Vec::new();

    for target in targets {
        let rt = match &target.node {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => return Err(unsupported("输出列表里的这种项")),
        };
        let value = rt
            .val
            .as_deref()
            .ok_or_else(|| unsupported("空的输出项"))?;

        // 分组查询里 `*` 没法一般化地展开（要靠 GROUP BY 判断哪些列合法），
        // 与其猜，不如明说
        if let Some(node::Node::ColumnRef(c)) = &value.node {
            if star_ref(&c.fields).is_some() {
                return Err(unsupported(
                    "分组查询里的 `*`（请把每一列都写进 GROUP BY 或聚合函数里）",
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

/// 降级一个字面量。
///
/// `expected` 是上下文给出的目标类型：`'Alice'` 在没有上下文时默认 TEXT，在
/// `WHERE name = 'Alice'` 里则按 `name` 的类型解释。
fn lower_const(c: &AConst, expected: Option<&DataType>, p: &mut ProgBuilder) -> Result<Reg> {
    if c.isnull {
        // `NULL` 关键字：无类型，靠上下文定型（比较、INSERT 的目标列等）
        return Ok(p.value(Value::Null));
    }

    // 有上下文时，按**隐式转换规则**把字面量定成目标类型：
    //
    // * 字符串字面量是 PostgreSQL 说的"未知类型"，可以定成任何类型
    //   （`name = 'Alice'`、`ts > '2000-01-01'`、`'5'::int` 都靠这条）；
    // * 整数只能落到 INT 或 NUMERIC（int → numeric 是隐式转换）；
    // * 小数只能落到 NUMERIC。
    //
    // 落不下去的（例如 `text_col = 5`）**不硬塞**，让它按本来类型去比，
    // 于是 `verify` 会报出"两侧类型不同"——这正是 PostgreSQL 的行为
    // （`operator does not exist: text = integer`）。
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

        // 没有上下文时，pg_query 把**超出 i32 的整数也塞进 Fval**：`SELECT
        // -2147483648` 是合法的 i32 却落在 Fval 里，而 `SELECT 2147483648` 才是真
        // 超范围。所以先按整数试一次——直接「见 Fval 就拒」会误杀 i32::MIN。
        Some(Val::Fval(f)) => match f.fval.parse::<i32>() {
            Ok(v) => Ok(p.int(v)),
            // 落不进 i32 的裸小数就是 NUMERIC —— 与 PostgreSQL 一致
            // （`SELECT 2.65` 的类型就是 numeric）。
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
        Some(Val::Bsval(_)) => Err(unsupported("位串字面量")),
        None => Err(unsupported("空常量")),
    }
}

/// 字面量的文本形式（整数也转成文本，好统一按目标类型再解析一次）。
fn literal_text(c: &AConst) -> Option<String> {
    match &c.val {
        Some(Val::Ival(i)) => Some(i.ival.to_string()),
        Some(Val::Fval(f)) => Some(f.fval.clone()),
        Some(Val::Sval(s)) => Some(s.sval.clone()),
        _ => None,
    }
}

/// 给字面量的解析失败补上"这是字面量"的上下文。
fn at_value(text: &str, e: DbError) -> DbError {
    match e {
        DbError::BadValue { ty, reason, .. } => DbError::BadValue {
            column: None,
            ty,
            value: format!("字面量 {}", text),
            reason,
        },
        other => other,
    }
}

/// 如果这个节点是个能解析的裸列引用，返回它的列类型。上下文定型要用。
fn bare_column_type(node: &Node, scope: &Scope) -> Option<DataType> {
    let c = match &node.node {
        Some(node::Node::ColumnRef(c)) => c,
        _ => return None,
    };
    let parts = ref_parts(&c.fields)?;
    let index = scope.resolve(&parts).ok()?;
    scope.column_type_at(index)
}

fn lower_aexpr(
    a: &AExpr,
    scope: &Scope,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let kind = AExprKind::try_from(a.kind).unwrap_or(AExprKind::Undefined);
    if kind != AExprKind::AexprOp {
        return Err(unsupported(match kind {
            AExprKind::AexprIn => "IN (...)".to_string(),
            AExprKind::AexprBetween | AExprKind::AexprNotBetween => "BETWEEN".to_string(),
            AExprKind::AexprLike => "LIKE".to_string(),
            AExprKind::AexprIlike => "ILIKE".to_string(),
            AExprKind::AexprSimilar => "SIMILAR TO".to_string(),
            AExprKind::AexprNullif => "NULLIF".to_string(),
            AExprKind::AexprDistinct | AExprKind::AexprNotDistinct => {
                "IS [NOT] DISTINCT FROM".to_string()
            }
            AExprKind::AexprOpAny | AExprKind::AexprOpAll => "= ANY / = ALL".to_string(),
            _ => format!(
                "运算符 `{}` 的这种用法",
                single_string(&a.name).unwrap_or_default()
            ),
        }));
    }

    let op_name = single_string(&a.name).ok_or_else(|| unsupported("这种运算符"))?;

    match (a.lexpr.as_deref(), a.rexpr.as_deref()) {
        // 二元
        (Some(l), Some(r)) => {
            let op = binop_of(&op_name)?;

            // 比较运算：一侧是裸列引用时，用**它的列类型**给另一侧的字面量定型。
            // 没有这一步，`WHERE ts = '2024-01-15'` 里的字面量会默认成 TEXT，
            // 与 TIMESTAMP 列比较时报类型不符。
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
                other => Err(unsupported(format!("一元运算符 `{}`", other))),
            }
        }
        _ => Err(unsupported(format!("运算符 `{}` 的这种用法", op_name))),
    }
}

fn lower_boolexpr(
    b: &BoolExpr,
    scope: &Scope,
    mut agg: Option<&mut AggLowering>,
    p: &mut ProgBuilder,
) -> Result<Reg> {
    let kind = BoolExprType::try_from(b.boolop).unwrap_or(BoolExprType::Undefined);

    match kind {
        BoolExprType::AndExpr | BoolExprType::OrExpr => {
            let op = if kind == BoolExprType::AndExpr {
                BinOp::And
            } else {
                BinOp::Or
            };

            let mut regs = Vec::with_capacity(b.args.len());
            for arg in &b.args {
                regs.push(lower_expr_in(arg, scope, None, agg.as_deref_mut(), p)?);
            }

            match regs.len() {
                0 => Err(unsupported("空的 AND / OR")),
                1 => Ok(regs[0]),
                _ => {
                    let mut acc = regs[0];
                    for reg in &regs[1..] {
                        acc = p.binary(op, acc, *reg);
                    }
                    Ok(acc)
                }
            }
        }

        BoolExprType::NotExpr => {
            if b.args.len() != 1 {
                return Err(unsupported("NOT 的参数个数不是 1"));
            }
            let inner = lower_expr_in(&b.args[0], scope, None, agg.as_deref_mut(), p)?;
            Ok(p.unary(UnOp::Not, inner))
        }

        _ => Err(unsupported("这种布尔表达式")),
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
        other => return Err(unsupported(format!("运算符 `{}`", other))),
    })
}

/// `ColumnRef.fields` → 一串字符串；含 `A_Star` 或别的节点则 `None`。
fn ref_parts(fields: &[Node]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(fields.len());
    for field in fields {
        match &field.node {
            Some(node::Node::String(s)) => out.push(s.sval.clone()),
            _ => return None,
        }
    }
    Some(out)
}

/// 识别 `*`（返回 `Some(None)`）与 `t.*`（返回 `Some(Some("t"))`）。
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

fn single_string(items: &[Node]) -> Option<String> {
    match items {
        [one] => match &one.node {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn unsupported(feature: impl Into<String>) -> DbError {
    DbError::Unsupported {
        feature: feature.into(),
    }
}

// ------------------------------------------------------------------ 作用域

/// 一个 FROM 项：名字、别名，以及它的列在扁平 schema 里的位置。
#[derive(Debug, Clone)]
struct Range {
    /// 限定名（`study.people` 或 `f`）。
    name: String,
    /// 别名（`AS t`）。
    alias: Option<String>,
    /// 这一项的列在扁平 schema 里的起始下标。
    offset: usize,
    /// 列定义。
    columns: Vec<Column>,
}

impl Range {
    /// 查询里写的 `qualifier` 能不能指向这一项。
    fn matches(&self, qualifier: &str) -> bool {
        if self.alias.as_deref() == Some(qualifier) {
            return true;
        }
        if self.name == qualifier {
            return true;
        }
        // `study.people.surname` 这种三段引用：用表名的最后一段匹配
        self.name.rsplit('.').next() == Some(qualifier)
    }

    /// 用于报错的限定名。
    fn qualified(&self) -> String {
        self.alias.clone().unwrap_or_else(|| self.name.clone())
    }
}

/// 降级期的名字作用域。顺序必须与 [`Plan::analyze`] 推导的 schema 一致。
#[derive(Debug, Clone, Default)]
struct Scope {
    ranges: Vec<Range>,
}

impl Scope {
    fn from_range(name: String, alias: Option<String>, columns: Vec<Column>) -> Self {
        Scope {
            ranges: vec![Range {
                name,
                alias,
                offset: 0,
                columns,
            }],
        }
    }

    /// 把另一个作用域接在后面（连接的输出 = 左 ++ 右）。
    fn concat(mut self, other: Scope) -> Self {
        for range in other.ranges {
            let offset = self.ranges.iter().map(|r| r.columns.len()).sum();
            self.ranges.push(Range { offset, ..range });
        }
        self
    }

    /// 扁平 schema。
    fn schema(&self) -> Vec<Column> {
        self.ranges
            .iter()
            .flat_map(|r| r.columns.iter().cloned())
            .collect()
    }

    /// 扁平下标 → 那一列的类型。上下文定型要用。
    fn column_type_at(&self, index: usize) -> Option<DataType> {
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

    /// 解一个列引用，返回**扁平下标**。
    fn resolve(&self, parts: &[String]) -> Result<usize> {
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
            0 => Err(DbError::UnknownColumn {
                name: name.to_string(),
                available: self.column_names(),
            }),
            1 => {
                let (ri, ci) = hits[0];
                Ok(self.ranges[ri].offset + ci)
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
}
