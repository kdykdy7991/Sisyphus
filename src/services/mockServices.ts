import { seedKnowledge } from '../data/mockData';
import type { BackupInspect, BackupRestoreResult, BackupSummary, ChatMessage, ImportImage, Knowledge, Settings, SyncExportSummary, SyncImportSummary, SyncInspectReport, WebDavConfig, WebDavSyncSummary, WebDavTestResult } from '../types';
import type { BackupService, ChatService, ImportService, KnowledgeService, SettingsService, SyncService, WebDavService } from './interfaces';
const wait = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
let knowledge = [...seedKnowledge];
let topics = [...new Set(knowledge.map(item => item.topic).filter(Boolean))];
const makeDraft = (image: ImportImage, index: number): Knowledge => {
  const existing = knowledge.find(item => item.topic === 'Redis');
  return {
    id: `draft-${image.id}-${index}`, question: index ? 'Redis 为什么使用单线程？' : 'Redis 为什么快？',
    answer: index ? 'Redis 核心命令使用单线程串行执行，避免了多线程上下文切换与锁竞争，并让命令天然保持原子性。网络 I/O 可由多线程处理。' : 'Redis 的高性能来自内存访问、高效数据结构、单线程命令执行、I/O 多路复用以及简洁的 RESP 协议。',
    domain: existing?.domain || '未分类', topic: existing?.topic || '', tags: ['Redis', '内存', 'I/O 多路复用'], followUps: ['Redis 6 为什么引入多线程？', 'I/O 多路复用如何工作？'], relatedIds: ['redis-fast', 'redis-single'], source: `截图导入 · ${image.name}`, createdAt: new Date().toLocaleString(), updatedAt: new Date().toLocaleString(),
  };
};
export const knowledgeService: KnowledgeService = {
  async list() { return [...knowledge]; },
  async listTopics() { return [...topics].sort((a, b) => a.localeCompare(b, 'zh-CN')); },
  async createTopic(name) { const value = name.trim(); if (value && !topics.includes(value)) topics.push(value); },
  async deleteTopic(name) { if (knowledge.some(item => item.topic === name)) return false; const before = topics.length; topics = topics.filter(topic => topic !== name); return topics.length < before; },
  async get(id) { return knowledge.find(item => item.id === id); },
  async search(query) { const q = query.toLowerCase(); return knowledge.filter(item => [item.question, item.answer, item.domain, item.topic, ...item.tags].join(' ').toLowerCase().includes(q)); },
  async save(item) { const index = knowledge.findIndex(entry => entry.id === item.id); if (index >= 0) knowledge[index] = item; else knowledge = [item, ...knowledge]; return item; },
  async recent(limit = 10) { return [...knowledge].slice(0, limit); },
  async delete(id) {
    const before = knowledge.length;
    knowledge = knowledge.filter(item => item.id !== id);
    return knowledge.length < before;
  },
  async clear() {
    const removed = knowledge.length;
    knowledge = [];
    topics = [];
    return removed;
  },
  async exportMarkdown(suggestedName, ids) {
    const selected = ids ? knowledge.filter(item => ids.includes(item.id)) : knowledge;
    const body = selected.map((item, index) => `## ${index + 1}. ${item.question}\n\n${item.answer}`).join('\n\n---\n\n');
    const blob = new Blob([`# Interview Kit 知识库\n\n${body}\n`], { type: 'text/markdown;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = suggestedName;
    link.click();
    URL.revokeObjectURL(url);
    return { path: suggestedName, itemCount: selected.length };
  },
};
export const importService: ImportService = {
  async extract(images, onProgress) {
    let next: ImportImage[] = images.map(image => ({ ...image, status: 'queued' })); onProgress(next); await wait(550);
    next = next.map(image => ({ ...image, status: 'extracting' as const })); onProgress(next); await wait(950);
    next = next.map((image, index) => ({ ...image, status: 'completed' as const, drafts: [makeDraft(image, index)] })); onProgress(next); await wait(350);
    next = next.map(image => ({ ...image, status: 'review' as const })); onProgress(next); return next;
  },
  async confirm(items) { for (const item of items) await knowledgeService.save({ ...item, id: `knowledge-${Date.now()}-${Math.random().toString(16).slice(2)}`, updatedAt: new Date().toLocaleString() }); },
  async suggestSimilarity() { return null; }, // mock: no real Similarity
  async updateExisting(draft, existingId) { const existing = await knowledgeService.get(existingId); if (!existing) return; await knowledgeService.save({ ...existing, question: draft.question, answer: draft.answer, domain: draft.domain, topic: draft.topic, tags: draft.tags, followUps: draft.followUps, updatedAt: new Date().toLocaleString() }); },
};
export const chatService: ChatService = {
  async ask(question, scope, _history: ChatMessage[]) {
    await wait(900); const refs = (await knowledgeService.search(question.includes('Redis') ? 'Redis' : '')).slice(0, 3);
    return { id: `a-${Date.now()}`, role: 'assistant', content: `基于${scope === 'all' ? '你的全部知识库' : '当前知识范围'}，可以从几个关键点理解这个问题。\n\nRedis 的高性能首先来自内存读写，避免了磁盘 I/O 的高延迟。其次，核心命令采用单线程串行执行，省去了上下文切换与锁竞争。配合经过优化的数据结构、I/O 多路复用和简洁的 RESP 协议，它可以用较低开销处理大量并发请求。\n\n需要注意：单线程指核心命令执行线程。Redis 6 之后可使用多线程处理网络 I/O，这并不改变命令串行执行的核心模型。`, citations: refs.map(item => item.id) };
  },
};
let settings: Settings = { apiBaseUrl: 'https://api.openai.com/v1', apiKey: '', chatModel: 'gpt-5', visionModel: 'gpt-5', visionContextTokens: 131072, visionMaxTokens: 32768, visionThinkingEnabled: true, visionReasoningEffort: 'medium', databaseLocation: '~/Library/Application Support/Interview Kit/interview.db' };
let llmProfiles = [{id:'default',name:'OpenAI · gpt-5',config:{...settings}}]; let activeProfileId='default';
export const settingsService: SettingsService = { async get() { return { ...settings }; }, async save(value) { settings = { ...value }; const p=llmProfiles.find(x=>x.id===activeProfileId);if(p)p.config={...settings};await wait(250); }, async profiles(){return{activeId:activeProfileId,profiles:llmProfiles.map(p=>({...p,config:{...p.config,apiKey:''}}))}},async createProfile(name){const id=`profile-${Date.now()}`;llmProfiles.push({id,name,config:{...settings,apiKey:''}});return id},async renameProfile(id,name){const p=llmProfiles.find(x=>x.id===id);if(!p)throw Error('配置不存在。');p.name=name.trim();},async switchProfile(id){const p=llmProfiles.find(x=>x.id===id)!;activeProfileId=id;settings={...p.config};return{...settings,apiKey:''}},async deleteProfile(id){if(llmProfiles.length<=1||id===activeProfileId)throw Error('请至少保留一份配置，并先切换后再删除。');llmProfiles=llmProfiles.filter(p=>p.id!==id)}, async testConnection() { await wait(800); return { ok: true, message: 'Mock 模式下无法测试真实连接。' }; } };

// Mock Backup: plain `npm run dev` (no Tauri) keeps the surface honest
// without any actual file IO. Useful for visual development of the Settings
// page; production code always goes through the Tauri implementation. The
// mock carries no application settings (the real backup also doesn't).
export const backupService: BackupService = {
  async pickSavePath() { return null; },
  async pickOpenPath() { return null; },
  async create() {
    await wait(300);
    return { path: '(mock) backup.ikbackup', formatVersion: 1, createdAt: new Date().toISOString(), knowledgeCount: knowledge.length, domainCount: 0 };
  },
  async inspect() {
    await wait(200);
    return { formatVersion: 1, app: 'Interview Kit (Mock)', createdAt: new Date().toISOString(), databaseSchemaVersion: 1, knowledgeCount: knowledge.length, domainCount: 0 };
  },
  async restore() {
    await wait(300);
    return { knowledgeCount: knowledge.length, domainCount: 0 };
  },
};

// Mock Sync: same intent as Mock Backup — no real file IO under
// `npm run dev`, but the surface matches the Tauri SyncService so the
// UI can be developed end-to-end. All counts are zeros, no merge
// actually happens; the production code goes through `syncService` in
// `tauriServices.ts`.
export const syncService: SyncService = {
  async pickSavePath() { return null; },
  async pickOpenPath() { return null; },
  async export() {
    await wait(300);
    return {
      path: '(mock) sync.iksync',
      formatVersion: 1,
      createdAt: new Date().toISOString(),
      itemCount: knowledge.length,
      activeCount: knowledge.length,
      deletedCount: 0,
    };
  },
  async inspect(): Promise<SyncInspectReport> {
    await wait(200);
    return {
      path: '(mock) sync.iksync',
      formatVersion: 1,
      app: 'Interview Kit (Mock)',
      createdAt: new Date().toISOString(),
      sourceDevice: '',
      itemCount: knowledge.length,
      activeCount: knowledge.length,
      deletedCount: 0,
      bytesRead: 0,
    };
  },
  async import(): Promise<SyncImportSummary> {
    await wait(300);
    return {
      path: '(mock) sync.iksync',
      formatVersion: 1,
      snapshotCreatedAt: new Date().toISOString(),
      snapshotItemCount: knowledge.length,
      snapshotActiveCount: knowledge.length,
      snapshotDeletedCount: 0,
      bytesRead: 0,
      stats: { inserted: 0, updated: 0, deleted: 0, skipped: knowledge.length, conflicts: 0 },
    };
  },
};

// Mock WebDAV: no real network under `npm run dev`, but the surface matches
// the Tauri WebDavService so the Settings UI can be developed end-to-end.
let webdavConfig: WebDavConfig = { url: '', username: '', password: '' };
export const webdavService: WebDavService = {
  async getConfig(): Promise<WebDavConfig> {
    // Never echo the password back, mirroring the real backend.
    return { ...webdavConfig, password: '' };
  },
  async saveConfig(config: WebDavConfig): Promise<void> {
    // An empty password keeps the stored one (just like the Rust side).
    webdavConfig = {
      url: config.url,
      username: config.username,
      password: config.password || webdavConfig.password,
    };
    await wait(200);
  },
  async testConnection(): Promise<WebDavTestResult> {
    await wait(800);
    if (webdavConfig.url.trim().length === 0) {
      return { success: false, latencyMs: 0, message: '尚未配置 WebDAV 地址。', errorKind: 'missingConfig' };
    }
    return { success: true, latencyMs: 120, message: '连接成功（Mock 模式，未发起真实请求）。', errorKind: '' };
  },
  async sync(): Promise<WebDavSyncSummary> {
    await wait(900);
    return {
      remoteExisted: false,
      downloadedItemCount: 0,
      uploadedItemCount: knowledge.length,
      stats: { inserted: 0, updated: 0, deleted: 0, skipped: knowledge.length, conflicts: 0 },
      syncedAt: new Date().toISOString(),
      retryCount: 0,
    };
  },
};

// Under mock there is no on-disk log folder; return a fake absolute path
// string so the UI's "logs are at: …" text still renders something useful
// (vite dev box, no real diagnostic behind it). `openDir` is a no-op here
// — clicking the button under mock should be harmless.
export const logsService = {
  async openDir(): Promise<void> { /* no-op under mock */ },
  async getDir(): Promise<string> { return '(mock) 内存模式，无日志目录'; },
};
