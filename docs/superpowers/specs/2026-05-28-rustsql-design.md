# RustSQL SQLite-like 数据库内核设计

日期：2026-05-28

## 1. 项目目标

构建一个 Rust 编写的 SQLite-like 数据库内核，但采用如下路线：

> 先做完整 SQL 外壳，再逐步把底层替换成真正的 pager / WAL / B+Tree 内核。

第一阶段优先获得：

- 可解析的 SQL
- 可执行的查询与写入
- 清晰稳定的模块边界
- 可替换的存储抽象

第二阶段再把底层升级成更接近 SQLite 的物理实现。

## 2. 总体架构

系统分为 6 个主要层次：

1. `sql::parser`
   - 负责 lexer、parser、AST
   - 把 SQL 文本转换成结构化语法树

2. `sql::planner`
   - 把 AST 转成逻辑执行计划
   - 第一版只做简单规则，不做复杂优化器

3. `sql::executor`
   - 按计划执行语句
   - 只调用抽象存储接口，不接触页、日志、B+Tree 细节

4. `engine`
   - 定义系统稳定边界
   - 包含 `CatalogStore`、`TableStore`、`IndexStore`、`TransactionManager`、`StorageEngine`

5. `storage_v1`
   - 第一阶段可运行存储实现
   - 用于快速跑通 SQL 全链路
   - 实现简单、可持久化、便于调试

6. `storage_v2`
   - 第二阶段真实内核
   - 逐步引入 pager、page format、WAL、B+Tree、catalog table

## 3. 核心设计原则

整个项目遵守 4 条硬规则：

1. SQL 层只处理逻辑语义，不处理物理存储细节。
2. Executor 只能依赖 `engine::traits`，不能依赖具体存储实现。
3. `storage_v1` 与 `storage_v2` 必须共用统一数据模型。
4. 后续替换底层时，尽量做到 SQL 层零或极少改动。

## 4. 模块与目录组织

建议采用单 crate、清晰模块划分：

```text
rustsql/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── main.rs
    ├── db.rs
    ├── sql/
    ├── engine/
    ├── storage/
    │   ├── memory.rs
    │   ├── v1/
    │   └── v2/
    ├── common/
    └── repl/
```

模块职责：

- `sql/`：SQL 前端与执行逻辑
- `engine/`：统一抽象接口与核心错误/事务边界
- `storage/`：不同存储后端实现
- `common/`：共享类型、Schema、Row、Value、错误定义
- `repl/`：命令行交互展示层

## 5. 第一阶段支持的数据模型

### 5.1 Value

第一版仅支持：

- `Null`
- `Integer(i64)`
- `Text(String)`
- `Boolean(bool)`

### 5.2 ColumnType

第一版仅支持：

- `Integer`
- `Text`
- `Boolean`

### 5.3 约束

第一版先支持：

- `NOT NULL`
- `PRIMARY KEY`

后续再扩展：

- `UNIQUE`
- `DEFAULT`
- 更复杂类型系统
- SQLite type affinity 风格兼容

### 5.4 Row

第一版采用：

- `Row = Vec<Value>`

目标是先保证全链路简单稳定，而不是一开始就追求极致紧凑编码。

### 5.5 Schema

包含：

- 表名
- 列定义
- 主键列信息
- 索引元数据

Schema 是 SQL 层、planner、executor、storage 的共享核心结构。

## 6. SQL 能力范围

### 6.1 第一阶段支持的语句

- `CREATE TABLE`
- `CREATE INDEX`
- `INSERT INTO`
- `SELECT ... FROM ... WHERE ...`
- `BEGIN`
- `COMMIT`
- `ROLLBACK`

### 6.2 SELECT 的限制

第一版只支持：

- 单表查询
- `SELECT *` 或列列表
- 可选 `WHERE`
- `WHERE` 先只支持：
  - `col = literal`
  - `col > literal`
  - `col < literal`

### 6.3 第一阶段明确不做

- `JOIN`
- `GROUP BY`
- `ORDER BY`
- 聚合函数
- 子查询
- 复杂表达式树
- 多表优化器

## 7. 执行计划设计

Planner 输出的核心计划节点：

- `CreateTable`
- `CreateIndex`
- `Insert`
- `SeqScan`
- `IndexScan`
- `BeginTxn`
- `CommitTxn`
- `RollbackTxn`

### 7.1 计划选择规则

第一版 planner 规则如下：

- 如果 `WHERE` 命中已定义索引，并且谓词形式受支持，则生成 `IndexScan`
- 否则生成 `SeqScan`

目标不是做高级优化，而是先把计划层和执行层的边界建立起来。

## 8. 存储抽象接口

Executor 不直接操作文件或页，而是通过抽象接口与存储层交互。

### 8.1 CatalogStore

负责：

- 创建表定义
- 创建索引定义
- 查询 schema
- 查询索引元数据

### 8.2 TableStore

负责：

- 插入行
- 顺序扫描
- 按逻辑 `RowId` 读取

### 8.3 IndexStore

负责：

- 创建索引
- 写入索引项
- 按 key 查找匹配行
- 后续支持范围扫描

### 8.4 TransactionManager

负责：

- `begin`
- `commit`
- `rollback`

### 8.5 StorageEngine

统一组合上述能力，供 executor 使用。

## 9. 事务设计

项目总体目标采用 WAL 方向，但因整体路线为 SQL 外壳优先，所以事务分阶段推进。

### 9.1 第一阶段事务语义

- 单进程
- 单写事务
- `BEGIN / COMMIT / ROLLBACK` 可用
- 先保证事务接口和行为语义正确
- 并发控制不作为首阶段重点

### 9.2 第二阶段事务升级

在 `storage_v2` 中逐步引入：

- pager
- WAL append
- commit marker
- recovery
- 脏页刷盘策略
- 更清晰的 crash consistency 保障

## 10. 两阶段存储路线

### 10.1 storage_v1：第一阶段

目标：

- 先把数据库跑起来
- 可持久化
- 支持基础事务行为
- 支持索引查询
- 便于 executor 联调和端到端测试

### 10.2 storage_v2：第二阶段

目标：

- 替换为更正规的 SQLite-like 内核
- 引入：
  - `pager`
  - `page format`
  - `wal`
  - `btree`
  - `catalog table`
  - `record encoding`

原则：对 SQL 层尽量透明，只替换 `StorageEngine` 的内部实现。

## 11. 错误处理设计

建议从一开始按层定义错误：

- `SqlError`
- `PlanError`
- `StorageError`
- `TxnError`
- 顶层统一 `DbError`

这样能保证：

- 调试更清晰
- 测试更稳定
- CLI/REPL 对外错误展示统一

## 12. 测试策略

采用四层测试：

1. Parser/Planner 单测：验证 SQL -> AST -> Plan 正确。
2. Executor + Mock Storage 测试：验证执行器语义，不依赖真实存储。
3. Storage 后端测试：分别验证 `memory` / `storage_v1` / `storage_v2`。
4. 端到端 SQL 集成测试：通过 `Database::execute(sql)` 验证完整链路。

重点是不能只靠端到端测试，必须分层测试。

## 13. 第一阶段开发顺序

### Phase 0：项目骨架

- 建立 Cargo 项目
- 搭模块结构
- 定义公共类型与 trait

### Phase 1：Parser + AST

- 支持基本 SQL 语法
- 完成 parser 单测

### Phase 2：Planner

- AST -> Plan
- 完成 planner 单测

### Phase 3：内存后端

- 先做 `memory` 实现
- 跑通 SQL 全链路

### Phase 4：storage_v1

- 加入最小持久化
- 支持 catalog/table/index 落盘
- 事务语义初步可用

### Phase 5：storage_v2

- pager / page / WAL / B+Tree
- 底层替换升级

## 14. 第一阶段成功标准

至少应稳定支持以下流程：

```sql
BEGIN;
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
CREATE INDEX idx_users_id ON users (id);
INSERT INTO users VALUES (1, 'alice');
INSERT INTO users VALUES (2, 'bob');
COMMIT;

SELECT * FROM users WHERE id = 1;
```

并满足：

- 能正确返回查询结果
- `ROLLBACK` 能撤销未提交写入
- 至少一类索引查询有效
- 到 `storage_v1` 阶段后，重启仍能恢复 schema 与数据
- `cargo test` 全部通过
- SQL 层不依赖具体存储实现

## 15. 推荐结论

最终推荐路线：

> 先建立 SQL 前端、规划层和执行层，使用稳定 trait 抽象隔离存储；先以 `memory` 和 `storage_v1` 跑通产品形态，再无缝升级到底层 pager/WAL/B+Tree 的 `storage_v2`。

这是当前最符合项目目标且最可落地的方案。
