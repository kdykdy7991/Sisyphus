# Sisyphus — Phase 1

面向个人使用的桌面面试知识工作台。本阶段提供 Tauri 2 桌面壳、React 前端与完整 TypeScript Mock 交互，不包含数据库、OCR 或真实模型调用。

## 启动

```bash
npm install
npm run tauri dev
```

需要 Node.js 18+、Rust stable，以及 Tauri 2 对应的系统依赖。仅调试前端可运行：

```bash
npm run dev
```

生产前端构建：

```bash
npm run build
```

## 已实现页面

- 首页：全局搜索、知识概览、知识领域、最近阅读、最近导入与快捷入口。
- 知识库：领域/主题浏览、关键词搜索、知识列表与空状态。
- Knowledge Detail：文章式阅读、收藏、原地编辑、标签、追问、相关问题、来源、备注和时间信息。
- 导入知识：支持微信公众号等公开网页链接和图片；单个来源可提取多组问答，并提供提取状态、Review、草稿切换、全字段编辑、Pass、Confirm 与批量操作。
- 对话：Knowledge Scope、提问、Loading、文章式 Mock Answer、引用与相关问题；引用可跳转 Knowledge Detail。
- 设置：模型参数、连接测试、本地数据位置及四类数据操作的 Mock UI。

## 架构

页面仅依赖 Service Interface，Mock 实现集中在 `src/services/mockServices.ts`：

- `KnowledgeService`: `list / get / search / save`
- `ImportService`: `extract / extractUrl / confirm`
- `ChatService`: `ask`
- `SettingsService`: `get / save / testConnection`

共享业务类型位于 `src/types.ts`，初始知识数据位于 `src/data/mockData.ts`。确认导入会经 `ImportService` 写入 Mock Knowledge Store，并刷新全局上下文。

## Mock 数据结构

- `Knowledge`: id、question、answer、domain、topic、tags、followUps、relatedIds、source、createdAt、updatedAt、favorite。
- `ImportImage`: id、name、url、status、drafts；状态依次为 ready、queued、extracting、completed、review。
- `ChatMessage`: id、role、content、citations。
- `Settings`: apiBaseUrl、apiKey、chatModel、visionModel、databaseLocation。

## 与设计稿的差异

- 设计稿中的原始截图缩放按钮当前为视觉控件，未实现真实画布缩放。
- 富文本编辑器在 Phase 1 使用纯文本编辑区，保留 Markdown 标题渲染，不包含完整富文本命令。
- Mock 状态仅在当前应用会话内存中存在，重启后恢复种子数据。
- 窗口使用操作系统原生标题栏；设计稿左上角交通灯作为品牌视觉保留在侧栏。
- 字体优先使用 Noto Serif SC / Noto Sans SC，离线不可用时回退系统宋体与苹方。

## Phase 2 后端接口清单

1. Knowledge：分页/筛选列表、详情读取、全文搜索、创建、更新、删除、收藏、领域/主题/标签管理、关联问题与最近阅读记录。
2. Import：导入任务创建、图片临时存储、任务状态查询/事件推送、Vision 提取、草稿读取与更新、单条/批量 Pass、单条/批量 Confirm、任务清理。
3. Chat：创建会话、历史会话列表、消息读取、基于 Knowledge Scope 的流式问答、引用返回与相关知识推荐。
4. Settings：模型配置读取/保存、安全存储 API Key、连接测试、数据库位置读取/迁移。
5. Data：数据库备份导出与恢复、Markdown 导出、JSON 导出、文件选择和保存对话框。
6. System：数据库迁移、应用数据目录、任务取消、统一错误结构和后台任务进度事件。

Phase 2 的前端适配层应实现现有 Service Interface，并在该实现内部调用 Tauri `invoke`/event；页面无需直接引用 Tauri API。
