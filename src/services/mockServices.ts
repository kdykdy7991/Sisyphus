import { seedKnowledge } from '../data/mockData';
import type { BackupInspect, BackupRestoreResult, BackupSummary, ChatMessage, ImportImage, Knowledge, Settings } from '../types';
import type { BackupService, ChatService, ImportService, KnowledgeService, SettingsService } from './interfaces';
const wait = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
let knowledge = [...seedKnowledge];
const makeDraft = (image: ImportImage, index: number): Knowledge => ({
  id: `draft-${image.id}-${index}`, question: index ? 'Redis 为什么使用单线程？' : 'Redis 为什么快？',
  answer: index ? 'Redis 核心命令使用单线程串行执行，避免了多线程上下文切换与锁竞争，并让命令天然保持原子性。网络 I/O 可由多线程处理。' : 'Redis 的高性能来自内存访问、高效数据结构、单线程命令执行、I/O 多路复用以及简洁的 RESP 协议。',
  domain: '后端开发', topic: 'Redis', tags: ['Redis', '基础原理'], followUps: ['Redis 6 为什么引入多线程？', 'I/O 多路复用如何工作？'], relatedIds: ['redis-fast', 'redis-single'], source: `截图导入 · ${image.name}`, createdAt: new Date().toLocaleString(), updatedAt: new Date().toLocaleString(),
});
export const knowledgeService: KnowledgeService = {
  async list() { return [...knowledge]; },
  async get(id) { return knowledge.find(item => item.id === id); },
  async search(query) { const q = query.toLowerCase(); return knowledge.filter(item => [item.question, item.answer, item.domain, item.topic, ...item.tags].join(' ').toLowerCase().includes(q)); },
  async save(item) { const index = knowledge.findIndex(entry => entry.id === item.id); if (index >= 0) knowledge[index] = item; else knowledge = [item, ...knowledge]; return item; },
  async recent(limit = 10) { return [...knowledge].slice(0, limit); },
  async clear() {
    const removed = knowledge.length;
    knowledge = [];
    return removed;
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
let settings: Settings = { apiBaseUrl: 'https://api.openai.com/v1', apiKey: '', chatModel: 'gpt-5', visionModel: 'gpt-5', databaseLocation: '~/Library/Application Support/Interview Kit/interview.db' };
export const settingsService: SettingsService = { async get() { return { ...settings }; }, async save(value) { settings = { ...value }; await wait(250); }, async testConnection() { await wait(800); return { ok: true, message: 'Mock 模式下无法测试真实连接。' }; } };

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
