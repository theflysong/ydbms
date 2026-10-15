# AGENTS.md —— `src/usr`

## 职能

**用户与库**：谁能进、进哪个库、库在磁盘上的什么位置。

- `mod.rs` —— [`User`](mod.rs) / [`UserStore`](mod.rs)（`users.json`）、
  [`UserStore::authenticate`](mod.rs)、`AuthError`、`now_unix`。
- `hash.rs` —— 口令的加盐哈希：`sha256(salt ‖ password)`、`/dev/urandom` 取盐、
  十六进制的进出。
- `service.rs` —— 服务目录 [`ServiceDir`](service.rs)（`users.json` + 各库目录）、
  库元数据 [`Catalog`](service.rs)（`<库>/catalog.json`）、打开着的库
  [`Database`](service.rs)（目录 + 元数据 + **单写者锁**）。

`ysql` 的命令行入口在 [`cmd::cli`](../cmd/cli.rs)；引擎侧的提交点在
[`cmd::engine::Session`](../cmd/engine.rs)。

## 磁盘契约

```text
<server-dist>/                    默认 ~/.ydbms（--server-dist 可换）
├── users.json                    {version, users:[{name, salt, password_hash, is_admin, created_unix}]}
└── <库>/
    ├── catalog.json              {version, name, owner, created_unix}
    ├── .lock                     单写者锁（flock）
    ├── <表>.csv                  数据（表名就是文件名主干，可能带 schema 前缀）
    └── <表>.schema.json          模式
```

- **`users.json` 与 `catalog.json` 的字段名、`version`、`is_admin` 都属于磁盘契约**：
  改名或改结构就是改文件格式。**新增字段一律 `#[serde(default)]`**，保证老文件读得回来。
- 版本不认识就**报错**，别按当前格式硬读。
- 库目录里**缺 `catalog.json` 不自动补**：owner 无从知道，补一个等于伪造 owner。
- 表的落盘/读回**只在 [`pers`](../pers/AGENTS.md)**：这里只把"库目录 + 表名"接起来，
  文件名一律由 `TablePath::new(dir, 表名)` 推。
- **表名就是文件名主干**（`<表>.csv`），所以 [`is_file_stem_safe`](../db/table.rs) 判为
  不安全的名字（空、`.`、`..`、含 `/`、`\`、NUL）在这里**再挡一次**。`ir` 建表时已经拒过
  一遍，这一遍是**有意的**、不是可以合并的重复：`ir` 那一道是为了早报错（建表时就告诉
  用户），这一道是**安全边界**——真正 `Path::join` 的是这里，放过去就等于允许
  `CREATE TABLE "../别人的库/evil"` 把文件写到别人的库目录（乃至服务目录）里去。
  库名（目录名）同理，见 `checked_name`。

## 特殊规则

### 口令：不存明文，也不假装它是 KDF

- `users.json` 就在用户主目录里，**明文比加盐哈希危险得多**——这条不许破。
- 存的是 `sha256(salt ‖ password)` 的小写十六进制，盐每用户 16 字节、来自
  `/dev/urandom`；读不到随机源就**报错**，不要退化成时间戳那种可预测的"随机"。
- 这是**加盐单轮**哈希，**没有迭代拉伸**，抗暴力枚举不如 bcrypt / argon2。
  要升级就换算法并借 `users.json` 的 `version` 迁移——**别只把这段注释删掉**。
- 比较走 `hash::verify`（常量时间），别写成 `==` 直接比字符串。

### 库与用户的口径（本版有意做的减法）

- **只做认证 + "owner（或管理员）才能连"**。`GRANT` / `REVOKE`、多用户共享、
  `CREATE USER` 都**明确不做**：用户要么由首次启动的 bootstrap 建出来，要么手工编辑
  `users.json`。要把这些做进来，先想清楚"授权信息存哪儿"。
- **一个目录 = 一个库**：库名只能是一个目录名（空、`.`、`..`、含 `/` 或 `\` 一律拒绝，
  否则就跑到服务目录外面去了）。
- `search_path` **不落盘**（它是会话状态）：新进程连回一个库之后，非限定表名解析不到，
  要写限定名或自己 `SET search_path`。这是已知偏差，别当成 bug 去"顺手修"。

### 单写者锁

- 每个库目录一把 `flock`（`File::try_lock`，标准库能力，**不加依赖**）。
- **先上锁、再载入表**：反过来的话载入会在半途被另一个写者改掉文件。
- 锁随句柄走，进程退出（含 `kill -9`）内核就释放——**没有陈旧锁文件要清理**，
  也不要做"锁文件里有 pid 就删掉"那种危险的"修复"。
- 本版**不做多进程协调**：第二个写者直接连不上。宁可连不上，也不要写坏数据。
- `DROP DATABASE` 之前要**探一次锁**（别人正连着就不删），并拒绝删自己正在用的库。

### 报错

- 服务目录这一层的错误统一用 [`DbError`](../db/table.rs)：
  `UnknownDatabase` / `DuplicateDatabase` / `Denied{message}`（正文写中文缘由），
  文件坏了用 `Format`、IO 用 `Io`。
- **认证失败用 `AuthError`**（`Display` 直接是中文，`ysql` 原样打出来）：
  `UnknownUser` / `BadPassword` / `Corrupt`。
- 拒绝的时候要把**为什么**说清楚（谁的库、owner 是谁、缺哪个文件），
  这是这一层唯一的使用者体验。
