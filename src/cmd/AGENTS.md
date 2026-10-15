# AGENTS.md -- `src/cmd`

## 职能

**执行引擎**:接受**任务**(SQL 与元命令混在一起的文本),驱动 `ir/` 里的 SQL 部分,
管住两路输出.任务的来源只有三种,在引擎里**没有区别**:

- 交互式终端敲进来的一段(`repl.rs`);
- `\i <文件>` 读进来的内容(`Session::run_file`)--`ysql -f <脚本>` 也走它;
- 元命令本身(`metacmd.rs`).

- `engine.rs` -- [`Session`](engine.rs):状态(符号表,两路输出,`\i` 递归深度,
  `\timing` / `ON_ERROR_STOP` 两个开关,服务模式下的提交点),输入切分,元命令的**语义**,
  SQL 的执行与报告.
- `metacmd.rs` -- 元命令的**语法**:把一行拆成命令名与参数,认不出来的也照样返回
  `MetaCmd::Unknown`.命令名只有一份(`NAMES`),补全与错误提示都从它取.
- `repl.rs` -- 交互式终端:rustyline 接线,[`is_complete`](repl.rs)(多行判定),非 tty 回退.
- `complete.rs` -- Tab 补全的候选,**纯逻辑**,不认识终端.
- `cli.rs` -- `ysql` 的命令行:参数解析(纯函数),认证,决定连哪个库,退出码.

`main.rs` 只把控制权交给 `cmd::repl::run`(内存模式),`src/bin/ysql.rs` 交给
`cmd::cli::run`(服务模式);真正的状态都在这儿.

## 特殊规则

### 内存模式与服务模式:同一个 `Session`

- `Session::new` / `with_sinks` 是**内存**会话(数据退出即丢);`Session::open` /
  `open_with_sinks` 绑着磁盘上的一个库(`usr::service::Database`,已经上过单写者锁),
  并把库级默认值(`catalog.json` 的 `settings.search_path`)套成会话的初始
  `search_path`.两种模式在引擎里**只有 `store` 一个字段的差别**,别为服务模式另开一套循环.
- **一条语句 = 一个隐式事务**(`ir::stmt::execute_stmt_with` 那条路):`run_sql` 每跑完
  一条就调一次 `Session::commit`,事务里它自己空转,所以事务外是"写完就落盘",而
  `BEGIN; ... COMMIT;` 里一次都不落盘.**别把提交点挪回"一次 `run_with`"**:那会让
  `BEGIN` 变成一个没有意义的记号.
- **代价口径**:一条语句一个事务意味着没包事务的脚本里**每条写语句都整表重写一次**
  (`pers` 落盘是整表重写),几千条 INSERT 就看得出来.所以 `-f` / `\i` 的长装载要
  **包在 `BEGIN; ... COMMIT;` 里**(`filmdb.sql` 本来就是这样),这句话在 `ysql -h`,
  `engine.rs` 文件头与 `repl.rs` 的横幅里都要有.
- `Session::commit` 只写 `dirty` 里的表,并把"磁盘上有,符号表里已经没了"的表文件删掉
  (`DROP SCHEMA ... CASCADE` 靠这条落盘,**不需要语句报告删了谁**),最后交给
  `Database::commit`(影子分页 + `commit.json`,**一次提交是原子的**).**两张都空就
  一个字节的 IO 都不做**--少了这一句,一次纯 `SELECT` 也会去写 `commit.json`,
  库目录只读时会变成"查询成功却报提交失败".
- 脏标记只认 `Outcome::Created / Inserted / Updated` 三种(唯一的写路径),
  加新的写语句时**必须同时**把这个 `match` 补上,否则那条路悄悄不落盘.
- 提交失败**保留 `dirty`**(下次再试)并走 `Session::error`;磁盘上不会留下"改了一半"的
  库--失败点要么在暂存阶段,要么在推广阶段(`commit.json` 还在,下次打开时补完).
- 见到 `Outcome::Transaction { op: Rollback, applied: true, .. }` 才把 `dirty` 清掉:
  那些改动真的被撤销了,再写回去就是白写一整张表.**`applied: false` 的那种
  (没有事务时的 `ROLLBACK`,只发一句警告)不许清**--那会把上一次提交失败留下的
  重试集一并抹掉,那批改动就再也不会落盘了.
- `CREATE DATABASE` / `DROP DATABASE` / `ALTER DATABASE ... SET` 的文件系统那一半在
  `usr`,引擎只把 `store` 作为 `ir::stmt::Databases` 递进去;内存模式由 `NoDatabases`
  明确拒绝,**事务里也一律拒绝**(撤销记录管不到服务目录).

### 文案:一律走 `tr!`

`engine.rs` / `cli.rs` / `repl.rs` / `metacmd.rs` 里那些给用户看的字(状态行,汇总,
错误,帮助,横幅)**不在这个文件里**:源文案在 `locales/zh-CN.yml`,译文在
`locales/en.yml`.改文案改那儿,别在代码里留硬编码中文--口径与键的规则见根
`AGENTS.md` 的 "六,文案与标点",`tests/i18n.rs` 会核对.

### 退出码与错误计数

- **报错一律走 `Session::error`**(它同时给 `failed` 计数);语句自己的失败走
  `Outcome::Err` 那条路,`run_sql` 里也要计入 `failed`.`ysql -f` 的退出码靠它,
  漏一处就会"报了错还退出 0".

### 与 `ir` 的分工:这里是引擎,那里是 SQL

- **引擎里不许有 SQL 知识**:解析,降级,执行都在 `ir/`(`Script` / `Plan` / `Prog`).
  这一层只管"哪一段是 SQL,哪一段是元命令,结果往哪一路写,出错要不要停".
- 依赖方向是 `cmd` → `ir`,**反过来不行**.要让引擎多做一件事,先问它是不是 SQL 的事.
- 想"逐条"干活(计时,出错就停)用 `ir::Script`:**解析一次,`run_one` 一条**.
  别在 `ir` 里塞回调,也别把 `cmd` 的策略塞进 `ir`.

### 两路输出是结构性的

`Session` 有两个独立的 [`Sink`](engine.rs),这是 `\o` 语义的核心:

- **`query_out`** -- 只装**查询结果表**(表头 + 数据行).
- **`messages`** -- 状态行,汇总,错误,以及 `\d`/`\echo`/`\ls` 等元命令的输出.

**`\o` 只换 `query_out`,永远不影响 `messages`.** 所以"`\o` 仅限于查询输出"这句话
是**结构上**成立的,不是靠约定.**不要把状态行,错误,汇总写进 `query_out`** --
那会立刻破坏 `\o` 的含义.

### 切分与元命令

- 元命令**只在行首**(trim 之后)才算.行中间的 `\` 是普通 SQL 文本.
- 元命令的**语法**只放 `metacmd.rs`(只拆词,不校验),**语义**只放 `engine.rs`
  (校验参数,写输出).不要在 `metacmd.rs` 里做路径检查或报错输出--那里没有
  输出去向可用;也不要在那里解释 `\timing yes` 这种取值(装原样的字符串即可).
- **`\!`(执行 shell)在脚本里禁用**,只在交互终端放行.这是有意的安全边界,
  不要为了方便打开.
- `\i` 有**递归深度上限**,防止脚本自我包含.放宽之前先想清楚会不会死循环.

### 错误与 `ON_ERROR_STOP`

- **报错一律走 `Session::error`**,别直接 `msg`:只有走它,`\set ON_ERROR_STOP on`
  才能真的"遇错就停".顺手 `msg` 一句错误,就会在那里漏掉停止.
- `Flow::Stop` 必须**一层层往上传**(`run_with` → `include_file` → `exec_meta`).
  `\i` 里设的开关要影响到外层脚本,这是 psql 的语义.
- **脚本那一路命中 `Stop` 之后不许再读输入**:`run_plain` 靠 `Session::stopped()`.
  交互终端里则只停"当前这一段提交",**不退出会话**--这是与 psql 的**有意差别**
  (数据只在内存里,为一次笔误把库丢掉太狠),要如实写进文档,别哪天"修"成退出.
- **事务是 `ir` 的事,引擎只管落盘**:`Flow::Stop` 打断的是"后面的语句",不是事务--
  事务还开着时 `Session::commit` 空转,数据留在内存里等用户 `COMMIT` / `ROLLBACK`
  (退出会话就等于丢掉它,与 PostgreSQL 断开连接一致).事务里失败的语句**不会**
  整句回滚(本版没有 `SAVEPOINT`),文档与输出都得如实说.
- 测试里"某条语句有没有跑"最可靠的证据是**表里的行数**,不是消息文本.

### 交互式终端(`repl.rs`)

- **终端只做引擎管不了的事**:行编辑与"这一句够了吗".别把 SQL 判断搬到这儿.
- 多行的判定([`is_complete`](repl.rs))有且只有这一处,行尾扫描在 `ir` 里
  (`has_top_level_terminator`,与 `statement_text` 那些词法活放一起).
- **非 tty 必须有回退**:管道/重定向没有终端可编辑,走 `run_plain`,并且 EOF 时
  要把攒着的交出去.`printf 'SELECT 1\n' | ydbms` 得能用.
- rustyline 在 `TERM=dumb` 上主动降级(没有补全)--这是它的行为,不要去"修".
- `main.rs` 只留一行调用:**不要把 REPL 状态放回 `main.rs`.**

### Tab 补全(`complete.rs`)

- **补全逻辑必须是纯函数**(`suggest(line, cursor, catalog)`):终端那边只转发候选,
  rustyline 自己决定"唯一候选直接写,多个补公共前缀,再按 Tab 列出来".于是补全能单测.
- 下标一律是**字节**下标,且必须落在**字符边界**上(中文表名不能切坏).
- 补全的候选**只能是引擎真正支持的东西**(关键字表,`metacmd::NAMES`,
  `ON_ERROR_STOP`).补出一个本版不认的写法比不补更糟.
- 候选每次现算(`Session::catalog`):上一条语句刚建出来的表,下一条就该能补出来.

### 元命令与 `EXPLAIN`

`EXPLAIN` 不是元命令(不以 `\` 开头),它只是交互终端的一个显示便利,所以脚本里
用不了.新增这类"终端便利"时,想清楚它该不该在脚本里可用.
