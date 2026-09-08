# 后续计划 (Next-time TODO)

> 当前阶段（截至 2026-09-06）已完成：知识库单条删除 + 失败诊断、Vision 排版规则重写、
> LLM reasoning 模型 `<think>` 块解析修复。以下为下一轮迭代待办。

---

## LLM 配置支持保存后切换（多配置，不覆盖旧的）

**现状**

- `src-tauri/src/config.rs` 的 `ApiConfig` 为**单份**结构，`save()` 直接覆盖
  `config.json`，内存态 `ApiConfigState` 也只持有一份。
- 没有“配置名/ID + 多份配置 + 当前启用项”的模型，因此新增一个配置会把旧的顶掉。

**目标**

- 引入**多份 LLM 配置（profile）** 的概念：每份含一个唯一标识/名称 + `ApiConfig`
  字段（baseUrl / key / chatModel / visionModel）。
- 支持：新增、保存、在已保存配置之间**切换**、删除（至少保留一份）、设置“当前启用”。
- 切换后即时生效：内存态与 `config.json`（或新的 `configs.json` / 索引文件）同步更新，
  且需与现有“空 key 视为保留原值”的合并逻辑兼容（避免切换时清空 key）。

**验收**

- 新增配置 B 后，配置 A 仍存在、可切回且字段完整；
- 切换配置后新对话使用所选配置的模型与地址；
- 配置含密钥，仍按既有 0600 权限落盘，不泄露到备份/同步（`.ikbackup` / `.iksync`）。

---
