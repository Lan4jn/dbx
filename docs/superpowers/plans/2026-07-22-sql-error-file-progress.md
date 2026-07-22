# SQL 错误定位与文件执行进度实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 在查询结果区展示可定位的 SQL 错误行列和片段，并让 SQL 文件执行每 5 秒按文件字节更新进度和当前语句片段。

**架构：** 前端扩展现有 `sqlDiagnostics.ts`，生成与 UI 无关的错误位置和行片段，再由 `ContentArea.vue` 使用 `resultSourceRange` 映射到编辑器文档。SQL 文件执行在 `dbx-core` 中扩展进度事件，并用 `tokio::select!` 在等待单条 SQL 时发送 5 秒心跳；前端使用独立纯函数维持单调百分比。

**技术栈：** Vue 3、TypeScript、Vitest、CodeMirror 6、Rust、Tokio、Tauri。

---

## 文件结构

- `apps/desktop/src/lib/sql/sqlDiagnostics.ts`：解析行列、绝对字符位置，生成错误行片段和 caret。
- `apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts`：覆盖数据库错误格式、边界和 UTF-8/多行偏移。
- `apps/desktop/src/components/ui/ErrorBanner.vue`：为居中错误状态提供详情插槽。
- `apps/desktop/src/components/grid/DataGrid.vue`：把错误详情插槽透传给结果区。
- `apps/desktop/src/components/layout/ContentArea.vue`：组装错误详情并将局部位置映射到编辑器范围。
- `apps/desktop/src/components/editor/QueryEditor.vue`：暴露聚焦、选择和滚动到范围的方法。
- `apps/desktop/src/lib/sql/sqlFileProgress.ts`：计算单调字节百分比和当前语句显示文本。
- `apps/desktop/src/lib/__tests__/sql/sqlFileProgress.spec.ts`：覆盖 99% 上限、终态 100%、不倒退和 fallback。
- `apps/desktop/src/components/sql-file/SqlFileExecutionDialog.vue`：消费字节进度和当前语句片段。
- `apps/desktop/src/lib/backend/tauri.ts`：扩展 `SqlFileProgress` 可选字段。
- `crates/dbx-core/src/sql.rs`：扩展 Rust 进度结构并提供 UTF-8 安全的 2 KiB 片段函数。
- `crates/dbx-core/src/sql_file_import.rs`：跟踪文件读取字节，在语句执行期间发送 5 秒心跳。
- `src-tauri/src/commands/sql_file.rs`、`crates/dbx-web/src/routes/sql_file.rs`：继续转发扩展后的兼容事件结构。

### 任务 1：SQL 错误位置纯函数

**文件：**
- 修改：`apps/desktop/src/lib/sql/sqlDiagnostics.ts`
- 创建：`apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts`

- [ ] **步骤 1：编写失败测试**

覆盖 `line 2, column 4`、`at line 3`、`Position: 12`、`at character 8`、PostgreSQL `LINE n` 加 caret、越界位置和无法识别消息。期望 API：

```ts
expect(buildSqlErrorPresentation("syntax error at line 2, column 4", "select 1;\nselect x")).toMatchObject({
  line: 1,
  column: 3,
  lineText: "select x",
  caretColumn: 3,
});
```

- [ ] **步骤 2：运行测试验证失败**

运行：`pnpm test -- apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts`

预期：FAIL，`buildSqlErrorPresentation` 尚未导出。

- [ ] **步骤 3：实现最少解析与片段逻辑**

新增 `SqlErrorPresentation`，保留现有 `parseSqlErrorLocation` 调用兼容；绝对位置按一基偏移转换，行列和 caret 均夹紧到 SQL 范围，无法识别时返回 `null`。

- [ ] **步骤 4：运行测试验证通过**

运行：`pnpm test -- apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts`

预期：PASS。

- [ ] **步骤 5：提交**

```bash
git add apps/desktop/src/lib/sql/sqlDiagnostics.ts apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts
git commit -m "feat(sql): derive query error source locations"
```

### 任务 2：结果区错误详情和编辑器定位

**文件：**
- 修改：`apps/desktop/src/components/ui/ErrorBanner.vue`
- 修改：`apps/desktop/src/components/grid/DataGrid.vue`
- 修改：`apps/desktop/src/components/layout/ContentArea.vue`
- 修改：`apps/desktop/src/components/editor/QueryEditor.vue`
- 修改：`apps/desktop/src/i18n/locales/en.ts`
- 修改：`apps/desktop/src/i18n/locales/zh-CN.ts`
- 测试：`apps/desktop/src/lib/__tests__/tabs/tabPresentation.spec.ts`

- [ ] **步骤 1：先扩展失败测试验证局部偏移映射**

在 `tabPresentation.spec.ts` 中组合 `resultSourceRange` 和错误局部偏移，验证第二条语句的局部偏移能映射为完整文档位置，且编辑器内容变化后不返回错误范围。

- [ ] **步骤 2：运行测试确认失败或缺少组合辅助函数**

运行：`pnpm test -- apps/desktop/src/lib/__tests__/tabs/tabPresentation.spec.ts`

- [ ] **步骤 3：实现结果详情和定位命令**

`ErrorBanner` 新增 `details` 插槽，`DataGrid` 透传 `error-details`。`ContentArea` 使用活动结果的 `sourceStatement` 构造错误详情，展示一基行列、单行 SQL 和 caret；定位时将局部 offset 加到 `resultSourceRange(...).from`。`QueryEditor` 暴露：

```ts
function focusRange(range: { from: number; to: number }) {
  currentView.dispatch({
    selection: { anchor: from, head: to },
    effects: editorViewModule.EditorView.scrollIntoView(from, { y: "center" }),
  });
  currentView.focus();
}
```

无法可靠映射时不显示定位按钮，但仍显示原始错误。

- [ ] **步骤 4：运行定向测试和类型检查**

运行：`pnpm test -- apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts apps/desktop/src/lib/__tests__/tabs/tabPresentation.spec.ts && pnpm typecheck`

预期：全部通过。

- [ ] **步骤 5：提交**

```bash
git add apps/desktop/src/components apps/desktop/src/i18n apps/desktop/src/lib/__tests__/tabs/tabPresentation.spec.ts
git commit -m "feat(sql): show and locate query error details"
```

### 任务 3：SQL 文件进度纯函数和前端类型

**文件：**
- 创建：`apps/desktop/src/lib/sql/sqlFileProgress.ts`
- 创建：`apps/desktop/src/lib/__tests__/sql/sqlFileProgress.spec.ts`
- 修改：`apps/desktop/src/lib/backend/tauri.ts`

- [ ] **步骤 1：编写失败测试**

```ts
expect(nextSqlFileProgressPercent(70, { bytesProcessed: 50, totalBytes: 100 }, "running")).toBe(70);
expect(nextSqlFileProgressPercent(0, { bytesProcessed: 100, totalBytes: 100 }, "running")).toBe(99);
expect(nextSqlFileProgressPercent(99, { bytesProcessed: 100, totalBytes: 100 }, "done")).toBe(100);
expect(sqlFileCurrentStatement({ currentStatement: "SELECT 1", statementSummary: "SELECT" })).toBe("SELECT 1");
```

- [ ] **步骤 2：运行测试验证失败**

运行：`pnpm test -- apps/desktop/src/lib/__tests__/sql/sqlFileProgress.spec.ts`

- [ ] **步骤 3：实现纯函数和可选字段**

`SqlFileProgress` 增加 `bytesProcessed?: number | null`、`totalBytes?: number | null`、`currentStatement?: string | null`。运行态百分比限制为 0-99，并始终取历史最大值；缺少总字节时保留 8% 的现有运行提示。

- [ ] **步骤 4：运行测试验证通过**

运行：`pnpm test -- apps/desktop/src/lib/__tests__/sql/sqlFileProgress.spec.ts`

- [ ] **步骤 5：提交**

```bash
git add apps/desktop/src/lib/sql apps/desktop/src/lib/backend/tauri.ts
git commit -m "feat(sql-file): model byte-based progress"
```

### 任务 4：Rust 字节进度、语句片段和 5 秒心跳

**文件：**
- 修改：`crates/dbx-core/src/sql.rs`
- 修改：`crates/dbx-core/src/sql_file_import.rs`
- 测试：以上两个文件内的 `#[cfg(test)]` 模块

- [ ] **步骤 1：编写 UTF-8 片段和事件序列失败测试**

测试 2 KiB 以下不截断、中文字符不会被切断、超长片段以 `...` 结束；测试 `StatementDone` 立即发出；使用可控时钟/暂停 Tokio 时间验证执行超过 5 秒时出现 `Running` 心跳。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p dbx-core sql_file --lib`

- [ ] **步骤 3：扩展进度结构和构造器**

在 `SqlFileProgress` 增加带 `skip_serializing_if = "Option::is_none"` 的字段。新增 `sql_file_statement_fragment(statement, 2048)`，按 UTF-8 字节边界截断。

- [ ] **步骤 4：跟踪文件字节并发送心跳**

`SqlFileStreamDecoder` 记录实际读取字节；`execute_sql_file_path` 获取 `metadata.len()` 并把当前读取值传入语句执行。单条语句执行改为：

```rust
let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(5), Duration::from_secs(5));
tokio::pin!(execution);
let result = loop {
    tokio::select! {
        result = &mut execution => break result,
        _ = heartbeat.tick() => emit(running_progress_with_bytes_and_fragment(...)),
    }
};
```

取消、完成、失败后 future 退出，heartbeat 自动停止。`StatementDone` 加入立即事件集合。

- [ ] **步骤 5：运行 Rust 测试及 modern/legacy 检查**

运行：

```bash
cargo test -p dbx-core sql_file --lib
cargo check -p dbx
cargo check --manifest-path src-tauri-legacy/Cargo.toml
```

预期：全部退出码 0。

- [ ] **步骤 6：提交**

```bash
git add crates/dbx-core/src/sql.rs crates/dbx-core/src/sql_file_import.rs
git commit -m "feat(sql-file): emit byte progress heartbeats"
```

### 任务 5：SQL 文件窗口接入和最终验证

**文件：**
- 修改：`apps/desktop/src/components/sql-file/SqlFileExecutionDialog.vue`
- 修改：`apps/desktop/src/composables/useExportTracker.ts`
- 修改：`apps/desktop/src/i18n/locales/en.ts`
- 修改：`apps/desktop/src/i18n/locales/zh-CN.ts`

- [ ] **步骤 1：接入单调百分比和当前 SQL**

维护 `displayedProgressPercent`，每次事件调用 `nextSqlFileProgressPercent`；当前 SQL 使用 `currentStatement`，旧事件回退到 `statementSummary`。文本框保持现有最大高度和滚动，不渲染完整超长 SQL。

- [ ] **步骤 2：补全前端终态事件字段**

前端自行合成 done/error/cancelled 事件时复制最后一次 `bytesProcessed`、`totalBytes`、`currentStatement`，避免任务追踪器丢失新字段。

- [ ] **步骤 3：运行完整定向验证**

```bash
pnpm test -- apps/desktop/src/lib/__tests__/sql/sqlDiagnostics.spec.ts apps/desktop/src/lib/__tests__/sql/sqlFileProgress.spec.ts apps/desktop/src/lib/__tests__/tabs/tabPresentation.spec.ts
pnpm typecheck
pnpm build
cargo test -p dbx-core sql_file --lib
cargo check -p dbx
cargo check --manifest-path src-tauri-legacy/Cargo.toml
```

- [ ] **步骤 4：检查差异并提交**

```bash
git diff --check
git status --short
git add apps/desktop/src
git commit -m "feat(sql-file): display live statement progress"
```

