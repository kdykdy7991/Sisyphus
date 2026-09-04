import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Calendar } from 'lucide-react';
import { useApp } from '../AppContext';
import { knowledgeService } from '../services';
import type { Knowledge } from '../types';
import {
  Search,
  Command,
  ChevronRight,
  FileText,
  FolderOpen,
  Clock,
} from '../components/Icons';

// "相对时间" 标签：根据 ISO 字符串渲染成 "3 小时前 / 昨天 / 6 个月前" 等。
// 空串或无法解析时返回 ''，调用方自行决定降级展示。
function relativeTime(input: string | undefined | null): string {
  if (!input) return '';
  const t = Date.parse(input);
  if (isNaN(t)) return '';
  const diff = Date.now() - t;
  if (diff < 0) return '刚刚';
  const min = Math.floor(diff / 60000);
  if (min < 1) return '刚刚';
  if (min < 60) return `${min} 分钟前`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr} 小时前`;
  const day = Math.floor(hr / 24);
  if (day === 1) return '昨天';
  if (day < 7) return `${day} 天前`;
  if (day < 30) return `${Math.floor(day / 7)} 周前`;
  if (day < 365) return `${Math.floor(day / 30)} 个月前`;
  return `${Math.floor(day / 365)} 年前`;
}

// 简单天数差（向上取整）。同 relativeTime：不可解析时返回 Infinity，
// 让调用方按"未知"分支处理。
function daysSince(input: string | undefined | null): number {
  if (!input) return Infinity;
  const t = Date.parse(input);
  if (isNaN(t)) return Infinity;
  return Math.max(0, Math.floor((Date.now() - t) / 86400000));
}

export function HomePage() {
  const { knowledge } = useApp();
  const nav = useNavigate();
  const [q, setQ] = useState('');
  const [recent, setRecent] = useState<Knowledge[]>([]);
  const [review, setReview] = useState<Knowledge[]>([]);

  // "最近阅读"：按 lastReadAt / updatedAt 倒序取前 5 条。
  useEffect(() => {
    knowledgeService.recent(5).then(setRecent);
  }, []);

  // "值得回顾"：从未阅读优先，再补 14 天以上未读的旧内容，仍按时间正序。
  useEffect(() => {
    knowledgeService.list().then(all => {
      const never = all.filter(k => !k.lastReadAt);
      const stale = all
        .filter(k => k.lastReadAt && daysSince(k.lastReadAt) >= 14)
        .sort((a, b) => (a.lastReadAt! < b.lastReadAt! ? -1 : 1));
      const mixed: Knowledge[] = [...never, ...stale].slice(0, 5);
      setReview(mixed);
    });
  }, []);

  // 搜索过滤：输入时弹出候选，点击跳转到详情。
  const results = useMemo(() => {
    if (!q) return [];
    const needle = q.toLowerCase();
    return knowledge
      .filter(x =>
        [x.question, x.topic, x.domain, ...x.tags]
          .join(' ')
          .toLowerCase()
          .includes(needle)
      )
      .slice(0, 6);
  }, [q, knowledge]);

  // 顶部 4 个统计卡片的数据来源：
  //   total       - 全部知识数
  //   weeklyNew   - 本周新增（createdAt 在过去 7 天）
  //   topics      - 主题去重数
  //   recentlyRead- 本周阅读过（lastReadAt 在过去 7 天）
  //   neverRead   - 从未阅读（无 lastReadAt）
  //   *_diff      - 较上一周的变化量（绿色 ↑）
  const stats = useMemo(() => {
    const total = knowledge.length;
    const day = 86400000;
    const now = Date.now();
    const last7 = now - 7 * day;
    const prev7 = now - 14 * day;

    const newThisWeek = knowledge.filter(k => {
      const t = Date.parse(k.createdAt);
      return !isNaN(t) && t >= last7;
    }).length;
    const newPrevWeek = knowledge.filter(k => {
      const t = Date.parse(k.createdAt);
      return !isNaN(t) && t >= prev7 && t < last7;
    }).length;

    const readThisWeek = knowledge.filter(k => {
      const t = Date.parse(k.lastReadAt || '');
      return !isNaN(t) && t >= last7;
    }).length;
    const readPrevWeek = knowledge.filter(k => {
      const t = Date.parse(k.lastReadAt || '');
      return !isNaN(t) && t >= prev7 && t < last7;
    }).length;

    const topics = new Set(knowledge.map(k => k.topic).filter(Boolean)).size;
    const neverRead = knowledge.filter(k => !k.lastReadAt).length;

    return {
      total,
      weeklyNew: newThisWeek,
      newDiff: newThisWeek - newPrevWeek,
      topics,
      recentlyRead: readThisWeek,
      readDiff: readThisWeek - readPrevWeek,
      neverRead,
    };
  }, [knowledge]);

  return (
    <div className="home-layout">
      <section className="home-content">
        <div className="global-search">
          <Search />
          <input
            value={q}
            onChange={e => setQ(e.target.value)}
            placeholder="搜索问题、主题或关键词…"
          />
          <kbd>
            <Command /> K
          </kbd>
          {q && (
            <div className="search-popover">
              {results.length ? (
                results.map(x => (
                  <button
                    key={x.id}
                    onClick={() => nav(`/knowledge/${x.id}`)}
                  >
                    <FileText />
                    <span>
                      <b>{x.question}</b>
                      <small>
                        {x.domain} / {x.topic}
                      </small>
                    </span>
                  </button>
                ))
              ) : (
                <p className="empty-hint">没有找到相关知识</p>
              )}
            </div>
          )}
        </div>

        <div className="stats-cards">
          <div className="stat-card">
            <FileText />
            <div className="stat-num">{stats.total}</div>
            <div className="stat-label">知识问题</div>
            {stats.newDiff !== 0 && (
              <div className="stat-delta">
                ↑ {stats.newDiff > 0 ? `+${stats.newDiff}` : stats.newDiff} 本周
              </div>
            )}
          </div>
          <div className="stat-card">
            <FolderOpen />
            <div className="stat-num">{stats.topics}</div>
            <div className="stat-label">主题分类</div>
          </div>
          <div className="stat-card">
            <Calendar />
            <div className="stat-num">{stats.recentlyRead}</div>
            <div className="stat-label">本周阅读</div>
            {stats.readDiff !== 0 && (
              <div className="stat-delta">
                ↑ {stats.readDiff > 0 ? `+${stats.readDiff}` : stats.readDiff} 本周
              </div>
            )}
          </div>
          <button
            className="stat-card stat-link"
            onClick={() => nav('/knowledge?filter=never-read')}
          >
            <Clock />
            <div className="stat-num">{stats.neverRead}</div>
            <div className="stat-label">从未阅读</div>
            <ChevronRight className="stat-go" />
          </button>
        </div>

        <div className="recent-grid">
          <section className="recent-card">
            <header className="recent-head">
              <div>
                <h2>最近阅读</h2>
                <small>继续你的学习轨迹</small>
              </div>
              <button onClick={() => nav('/knowledge')}>
                查看全部 <ChevronRight />
              </button>
            </header>
            {recent.length === 0 ? (
              <p className="empty-hint">还没有阅读记录。</p>
            ) : (
              <ul className="recent-list">
                {recent.map(x => (
                  <li key={x.id}>
                    <button onClick={() => nav(`/knowledge/${x.id}`)}>
                      <FileText />
                      <span className="recent-title">{x.question}</span>
                      <span className="recent-meta">
                        {x.domain}　›　{x.topic}
                      </span>
                      <span className="recent-time">
                        {relativeTime(x.lastReadAt || x.updatedAt) || '未读'}
                      </span>
                      <ChevronRight />
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="recent-card">
            <header className="recent-head">
              <div>
                <h2>值得回顾</h2>
                <small>这些内容已经很久没看了</small>
              </div>
              <button onClick={() => nav('/knowledge?filter=review')}>
                查看全部 <ChevronRight />
              </button>
            </header>
            {review.length === 0 ? (
              <p className="empty-hint">暂时没有需要回顾的内容。</p>
            ) : (
              <ul className="recent-list">
                {review.map(x => {
                  const never = !x.lastReadAt;
                  const label = never
                    ? '从未阅读'
                    : relativeTime(x.lastReadAt!);
                  return (
                    <li key={x.id}>
                      <button onClick={() => nav(`/knowledge/${x.id}`)}>
                        <FileText />
                        <span className="recent-title">{x.question}</span>
                        <span className="recent-meta">
                          {x.domain}　›　{x.topic}
                        </span>
                        <span
                          className={`recent-time${
                            never ? ' is-warn' : ''
                          }`}
                        >
                          {label || '未读'}
                        </span>
                        <ChevronRight />
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
        </div>
      </section>
    </div>
  );
}