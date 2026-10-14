# AGENTS.md —— `src/ir`

## 职能

把 SQL 变成"要查什么"，然后执行它。**两层都是扁平的，全程没有递归**：

```text
SQL 文本
   │ pg_query::parse（PostgreSQL 的真实解析器）
   ▼
pg_query 裸语法树
   │ lower()      ← lower.rs   绑定：查表、解列名、类型检查
   ▼
Plan { nodes: Vec<Node> }        ← plan.rs   节点之间的边是「下标」
   │ 每个算子各带若干
   ▼
Prog { steps: Vec<Step> }        ← expr.rs   一小段寄存器程序
   ▼
RowSource（一行一行拉）           ← exec.rs   拉取式执行
   ▼
PhysTab                          ← db::source::materialize
```

- `symbol.rs` —— 名字 → 表的映射 [`SymbolTable`](symbol.rs)。
- `stmt.rs` —— 语句与脚本：[`Script`](stmt.rs)**解析一次、`run_one` 一条**（降级仍在
  执行时做——后面的语句依赖前面建出来的表）；`run_script` 就是它的循环包装。
  还放着一个给 REPL 用的词法小工具 `has_top_level_terminator`（"这段输入里有没有
  一句说完"），与 `statement_text` / `strip_leading_comments` 同属一类活。

## 特殊规则

### 执行是拉取式的，惰性边界就这几处

`execute_source` 造管道、`execute` 抽干 root。每个节点产出的是一个 `RowSource`
（见 [`../db/AGENTS.md`](../db/AGENTS.md)），**不是物化的表**。只有下面这几处会攒行：

1. 需要**随机访问**的算子（`Sort`、`Aggregate`）；
2. 嵌套循环连接的**内侧**（要重扫它）、哈希连接的 **build 侧**（抽干成「键 → 行」）；
   索引嵌套循环**不攒行**——它只建一份「键 → 行号」的索引，行按需从基表取；
3. 被**多个父节点**引用的节点（物化一次，各消费者拿一个游标）；
4. **最终结果**。

其它算子（`Scan`/`Filter`/`Project`/`Limit`）必须**流式**实现：`next()` 里一次只拉
输入的一行，别顺手 `drain`。拉取式的收益（`LIMIT` 之下不被求值、连接不为每一对
候选行拼行）全在这儿。

**管道只从 root 建**：从 root 走不到的节点不建源也不执行（静态校验仍然覆盖它们）。

`next()` 返回 `None` 是"结束**或**出错"，出错必须能由 `error()` 问到；复合算子不
抄输入的错误，而是 `self.input.error()` 往下问。

`Prog::eval_split` 是连接用的分裂输入求值：程序里的下标按 `左 ++ 右` 编，两半各取
各的，**不为每对候选行拼整行**。

### 连接键 / 索引键必须同族，索引键不能读列

哈希与索引都把 [`Value`](crate::db::table::Value) 当键，而 `Value::Int(1)` 与
`Value::Numeric("1")` **不是同一个值**——当键用会**安静地漏掉**本该匹配的行（嵌套循环
里的比较有 `widen_numeric` 兜着，键没有）。所以绑定器只挑**同族**的等值键，
[`Plan::analyze`](plan.rs) 再强制一遍；索引的键还**不许引用列**（它在没有输入行的情况下
求值）。这两条都属于「宁可不用这条路径，也不给安静的错答案」。

下推也有同样性质的两条规矩：谓词只能推进**一个** FROM 项的基表，`LEFT JOIN` 的
**可空侧一律不推**（补出来的 NULL 会让 `IS NULL` 这类谓词改语义，见 `lower.rs` 的
`Range::nullable`）。

### 两条不变量，不许破

- **I1（拓扑序）**：`Node::inputs` 里的下标必须**严格小于**节点自身的下标。它保证
  图无环，并让 schema 推导与执行都是一次正向 `for` 循环——深计划因此不递归、不爆栈。
  **不要引入"回头边"**，哪怕看起来方便。
- **I2（先定义后使用）**：`Step` 读到的寄存器必须在此前被写过。

两条都由 [`Plan::analyze`](plan.rs) 静态检查。**降级出来的计划一定要过 `analyze`。**
它不一致时会报 `IrInvalid` 而不是安静地读错列——绕过它就等于放弃这道保护。

### 类型与求值

- **布尔与真值是 IR 私有的**：`Tri`、`ScalarType::Bool` 都**不进** `DataType`、
  `Value`、磁盘格式。真值不能作结果列。
- **谓词的结果是三值**（`Tri::True` / `False` / `Unknown`），不是 `bool`。
  `WHERE`/`HAVING`/`CASE` 只认 `Tri::True`；`IS NULL` 是唯一永远给确定真值的东西。
- **比较只放行同族、`NULL`、以及 `INT`↔`NUMERIC`**。这是 PostgreSQL 里唯一的隐式
  放大（`int → numeric`）；`text = int`、`timestamp = text` 必须继续拒绝。
  放宽比较时**必须同时在运行时把 `Int` 转成 `Numeric`**——只改类型检查会让比较掉进
  `Value::cmp` 的「按大类排名」兜底，给出安静的错答案。
- **`CASE` 的两分支走同一条放大规则**：`INT` 与 `NUMERIC` 统一成 `NUMERIC`，
  一侧是 `NULL` 就取另一侧（**不放大**）。同样地，`unify_branches` 放行之后
  `widen_branches` 要在求值时**真的转**：否则投影列是 `NUMERIC`、值是 `Int`，
  物化时会被 `TypeMismatch` 顶回来。这条对照过 PostgreSQL 18.6：
  `pg_typeof(case when true then 1 else 1.5 end)` 就是 `numeric`。
- `Scope` 解出来的**扁平下标**必须与 `Plan::analyze` 推出来的 schema 完全一致。
  别名只影响限定名匹配，**不改列名**——改了就会不一致。

### 降级（`lower.rs`）

- **不认识的写法一律显式拒绝**（`DbError::Unsupported`），绝不猜。宁可拒绝，也不要
  给一个安静的错误答案。文件头列着当前仍拒绝哪些写法。
- `lower_expr_in` 的 `agg` 参数是**分组模式**：`Some` 时聚合调用变成对聚合输出的列
  引用、裸列引用去 `GROUP BY` 列表里找。
- **聚合抽取是一趟做的**：用 AST 节点地址认同一个聚合。不要改回"先收集再改写"的
  两趟写法——那要求两次遍历顺序完全一致，错了会**静默串位**。
- 分组查询里，**`SELECT` 列表与 `HAVING` 都必须在建 `Aggregate` 节点之前降级完**，
  因为 `HAVING` 里可能第一次出现某个聚合。

### 已知缺口：行级惰性 ≠ 表达式惰性

拉取式只管**行**。`Prog` 仍是**扁平且没有跳转**的，所以 `CASE` 的两个分支、
`AND`/`OR` 的两侧都会**先算完**——死分支里的运行时错误会冒出来。这是已知问题，
修法与取舍见仓库根的 `TODO.md` §2.2。改动 `Prog` 的求值模型前先读那一节。
