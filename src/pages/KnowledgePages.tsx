import { useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useApp } from '../AppContext';
import { knowledgeService } from '../services';
import type { Knowledge } from '../types';
import {
  Search,
  ChevronRight,
  ChevronLeft,
  FileText,
  Star,
  PenLine,
  MoreHorizontal,
  Plus,
  Save,
  Tags,
  FolderOpen,
  Clock,
  Filter,
} from '../components/Icons';
import { Button, Input, Tag, Textarea, Empty, Spinner } from '../components/UI';
import { Drawer } from '../components/Drawer';
import { useMediaQuery } from '../hooks/useMediaQuery';

const renderAnswer = (text: string) => {
  // Minimal markdown-ish renderer for the Vision-extracted `answer` field.
  // The system prompt instructs the model to lay out answers with headings,
  // ordered/unordered lists, blank-line paragraph breaks, and 2-4 space
  // indented sub-explanations. We only handle exactly those shapes:
  //   - `## `      -> <h2>
  //   - `1. ` ...  -> <ol><li>  (consecutive ordered items group)
  //   - `- ` ...   -> <ul><li>  (consecutive unordered items group)
  //   - indented   -> a small muted <p> attached to the previous list item
  //   - blank line -> <br> gap
  //   - anything else -> <p>
  // We deliberately do not pull in a markdown library; the model output is
  // intentionally simple and bounded by the prompt.
  const lines = text.split('\n');
  const out: React.ReactNode[] = [];
  let key = 0;
  let i = 0;
  const isOrdered = (s: string) => /^\d+\.\s+/.test(s);
  const isUnordered = (s: string) => /^[-*]\s+/.test(s);
  const isIndent = (s: string) => /^[ \t]+\S/.test(s);
  const stripOrdered = (s: string) => s.replace(/^\d+\.\s+/, '');
  const stripUnordered = (s: string) => s.replace(/^[-*]\s+/, '');

  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();

    if (trimmed === '') {
      out.push(<br key={key++} />);
      i++;
      continue;
    }

    if (line.startsWith('## ')) {
      out.push(<h2 key={key++}>{line.slice(3).trim()}</h2>);
      i++;
      continue;
    }

    if (isOrdered(trimmed)) {
      const items: { main: string; sub: string[] }[] = [];
      while (i < lines.length) {
        const cur = lines[i];
        const curTrim = cur.trim();
        if (curTrim === '') break;
        if (isOrdered(curTrim)) {
          items.push({ main: stripOrdered(curTrim), sub: [] });
          i++;
          continue;
        }
        if (items.length && isIndent(cur)) {
          items[items.length - 1].sub.push(curTrim);
          i++;
          continue;
        }
        break;
      }
      out.push(
        <ol key={key++}>
          {items.map((it, idx) => (
            <li key={idx}>
              {it.main}
              {it.sub.length > 0 && (
                <p className="sub">{it.sub.join(' ')}</p>
              )}
            </li>
          ))}
        </ol>
      );
      continue;
    }

    if (isUnordered(trimmed)) {
      const items: { main: string; sub: string[] }[] = [];
      while (i < lines.length) {
        const cur = lines[i];
        const curTrim = cur.trim();
        if (curTrim === '') break;
        if (isUnordered(curTrim)) {
          items.push({ main: stripUnordered(curTrim), sub: [] });
          i++;
          continue;
        }
        if (items.length && isIndent(cur)) {
          items[items.length - 1].sub.push(curTrim);
          i++;
          continue;
        }
        break;
      }
      out.push(
        <ul key={key++}>
          {items.map((it, idx) => (
            <li key={idx}>
              {it.main}
              {it.sub.length > 0 && (
                <p className="sub">{it.sub.join(' ')}</p>
              )}
            </li>
          ))}
        </ul>
      );
      continue;
    }

    // Plain paragraph: gather consecutive non-blank, non-list, non-heading
    // lines into a single <p> so we don't get one <p> per sentence.
    const para: string[] = [line];
    i++;
    while (i < lines.length) {
      const cur = lines[i];
      const curTrim = cur.trim();
      if (curTrim === '') break;
      if (curTrim.startsWith('## ')) break;
      if (isOrdered(curTrim) || isUnordered(curTrim)) break;
      para.push(cur);
      i++;
    }
    out.push(<p key={key++}>{para.join(' ')}</p>);
  }

  return out;
};

// 主题/领域筛选面板的内容。横屏与桌面仍放在常驻左栏；竖屏放进左侧抽屉，
// 两份渲染共用同一份数据与交互，不做 CSS 隐藏式的功能阉割。
function TopicPanelBody({
  domains,
  topics,
  domain,
  knowledge,
  onPickDomain,
  onPickTopic,
}: {
  domains: string[];
  topics: string[];
  domain: string;
  knowledge: Knowledge[];
  onPickDomain: (d: string) => void;
  onPickTopic: (t: string) => void;
}) {
  return (
    <div className="topic-panel-body">
      <div className="tabs">
        <b>按主题</b>
        <span>按标签</span>
        <span>按收藏</span>
      </div>
      <div className="domain-list">
        {domains.map(d => (
          <button
            className={domain === d ? 'active' : ''}
            onClick={() => onPickDomain(d)}
            key={d}
          >
            <ChevronRight />
            {d}
            <small>
              {d === '全部'
                ? knowledge.length
                : knowledge.filter(x => x.domain === d).length}
            </small>
          </button>
        ))}
        {topics.map(t => (
          <button className="topic" key={t} onClick={() => onPickTopic(t)}>
            {t}
            <small>{knowledge.filter(x => x.topic === t).length}</small>
          </button>
        ))}
      </div>
      <Button variant="ghost">
        <Plus /> 新建主题
      </Button>
    </div>
  );
}

export function KnowledgePage() {
  const { knowledge } = useApp();
  const nav = useNavigate();
  const [q, setQ] = useState('');
  const [domain, setDomain] = useState('全部');
  const [filterOpen, setFilterOpen] = useState(false);

  // 切到横屏/桌面双栏时左栏已经常驻，抽屉必须收起，避免同一内容重叠出现。
  const twoPane = useMediaQuery('(min-width: 700px)');
  useEffect(() => {
    if (twoPane) setFilterOpen(false);
  }, [twoPane]);

  const domains = ['全部', ...new Set(knowledge.map(x => x.domain))];
  const topics = [
    ...new Set(
      knowledge
        .filter(x => domain === '全部' || x.domain === domain)
        .map(x => x.topic)
    ),
  ];
  const filtered = knowledge.filter(
    x =>
      (domain === '全部' || x.domain === domain) &&
      [x.question, x.answer, x.topic, ...x.tags]
        .join(' ')
        .toLowerCase()
        .includes(q.toLowerCase())
  );

  const pickDomain = (d: string) => setDomain(d);
  const pickTopic = (t: string) => setQ(t);

  return (
    <div className="knowledge-browser">
      <aside className="topic-panel">
        <h1>知识库</h1>
        <TopicPanelBody
          domains={domains}
          topics={topics}
          domain={domain}
          knowledge={knowledge}
          onPickDomain={pickDomain}
          onPickTopic={pickTopic}
        />
      </aside>

      <section className="knowledge-list">
        <header>
          <div>
            <p>{domain}</p>
            <h2>{q ? `“${q}” 的搜索结果` : '全部知识'}</h2>
          </div>
          <div className="small-search">
            <Search />
            <input
              placeholder="搜索知识库…"
              value={q}
              onChange={e => setQ(e.target.value)}
            />
          </div>
        </header>

        {/* 竖屏筛选工具条：常驻左栏被 CSS 隐藏后，这里是唯一的筛选入口 */}
        <div className="knowledge-filter-bar">
          <span className="filter-current">
            当前：{domain}
            {q ? ` · 关键词“${q}”` : ''}
          </span>
          {q && (
            <button className="chip" onClick={() => setQ('')}>
              清除关键词
            </button>
          )}
          <button
            className="chip chip-primary"
            onClick={() => setFilterOpen(true)}
            aria-label="选择领域或主题"
          >
            <Filter /> 选择领域 / 主题
          </button>
        </div>

        {filtered.length ? (
          <div className="knowledge-rows">
            {filtered.map(x => (
              <button key={x.id} onClick={() => nav(`/knowledge/${x.id}`)}>
                <FileText />
                <span>
                  <b>{x.question}</b>
                  <small>{x.answer.slice(0, 82)}…</small>
                  <em>
                    {x.tags.map(t => (
                      <i key={t}>{t}</i>
                    ))}
                  </em>
                </span>
                <div>
                  <small>
                    {x.domain} / {x.topic}
                  </small>
                  <ChevronRight />
                </div>
              </button>
            ))}
          </div>
        ) : (
          <Empty
            icon={<Search />}
            heading="没有找到知识"
            text="尝试更换关键词或主题。"
          />
        )}
      </section>

      <Drawer
        open={filterOpen}
        onClose={() => setFilterOpen(false)}
        title="筛选领域 / 主题"
        side="left"
      >
        <TopicPanelBody
          domains={domains}
          topics={topics}
          domain={domain}
          knowledge={knowledge}
          onPickDomain={d => {
            setDomain(d);
            setFilterOpen(false);
          }}
          onPickTopic={t => {
            setQ(t);
            setFilterOpen(false);
          }}
        />
      </Drawer>
    </div>
  );
}

// 右栏内容。横屏/桌面常驻显示，竖屏进入"详情"抽屉。
function DetailRail({ item, related }: { item: Knowledge; related: Knowledge[] }) {
  const nav = useNavigate();
  return (
    <div className="rail-body">
      <div className="small-search">
        <Search />
        <input placeholder="搜索知识库…" />
      </div>
      <section>
        <h3>
          相关问题 <small>{related.length}</small>
        </h3>
        {related.map(x => (
          <button onClick={() => nav(`/knowledge/${x.id}`)} key={x.id}>
            <FileText />
            {x.question}
          </button>
        ))}
      </section>
      <section>
        <h3>所属主题</h3>
        <p>
          <FolderOpen /> {item.domain}　›　{item.topic}
        </p>
        <p>
          <Tags /> {item.tags.length} 个标签
        </p>
      </section>
      <section>
        <h3>来源</h3>
        <p>{item.source}</p>
      </section>
      <section>
        <h3>备注</h3>
        <Textarea placeholder="记录一些额外的想法…" rows={4} />
      </section>
      <div className="meta">
        <span>
          <Clock /> 创建时间
        </span>
        {item.createdAt}
        <span>最后编辑</span>
        {item.updatedAt}
      </div>
    </div>
  );
}

export function KnowledgeDetailPage() {
  const { id } = useParams();
  const { knowledge, refresh } = useApp();
  const nav = useNavigate();
  const [item, setItem] = useState<Knowledge>();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Knowledge>();
  const [railOpen, setRailOpen] = useState(false);

  const twoPane = useMediaQuery('(min-width: 700px)');
  useEffect(() => {
    if (twoPane) setRailOpen(false);
  }, [twoPane]);

  useEffect(() => {
    knowledgeService.get(id!).then(x => {
      setItem(x);
      setDraft(x);
    });
  }, [id, knowledge]);

  const related = useMemo(
    () =>
      item
        ? knowledge
            .filter(
              x =>
                item.relatedIds.includes(x.id) ||
                (x.topic === item.topic && x.id !== item.id)
            )
            .slice(0, 6)
        : [],
    [item, knowledge]
  );

  if (!item || !draft) return <div className="center"><Spinner /></div>;

  const save = async () => {
    await knowledgeService.save({
      ...draft,
      updatedAt: new Date().toLocaleString(),
    });
    await refresh();
    setItem(draft);
    setEditing(false);
  };

  return (
    <div className="detail">
      <div className="detail-top">
        <button onClick={() => nav('/knowledge')}>
          <ChevronLeft /> 知识库
        </button>
        <span>{item.domain}</span>
        <ChevronRight />
        <span>{item.topic}</span>
        <ChevronRight />
        <b>{item.question}</b>
        {/* 竖屏详情入口：右栏被 CSS 隐藏后，相关问题/主题/来源/备注/元数据都从这里进 */}
        <button
          className="rail-toggle"
          onClick={() => setRailOpen(true)}
          aria-label="查看详情信息"
        >
          <FileText /> 详情
        </button>
      </div>

      <article className="reader">
        <div className="reader-actions">
          <small># {knowledge.findIndex(x => x.id === item.id) + 1}</small>
          <span />
          <button
            onClick={() => setItem({ ...item, favorite: !item.favorite })}
            aria-label={item.favorite ? '取消收藏' : '收藏'}
          >
            <Star fill={item.favorite ? 'currentColor' : 'none'} />
            <span className="reader-action-text">
              {item.favorite ? '已收藏' : '收藏'}
            </span>
          </button>
          <button
            onClick={() => setEditing(!editing)}
            aria-label={editing ? '退出编辑' : '编辑'}
          >
            <PenLine />
            <span className="reader-action-text">编辑</span>
          </button>
          <button aria-label="更多操作">
            <MoreHorizontal />
          </button>
        </div>

        {editing ? (
          <div className="edit-form">
            <label>
              问题
              <Input
                value={draft.question}
                onChange={e => setDraft({ ...draft, question: e.target.value })}
              />
            </label>
            <label>
              核心回答
              <Textarea
                rows={16}
                value={draft.answer}
                onChange={e => setDraft({ ...draft, answer: e.target.value })}
              />
            </label>
            <div className="split">
              <label>
                领域
                <Input
                  value={draft.domain}
                  onChange={e => setDraft({ ...draft, domain: e.target.value })}
                />
              </label>
              <label>
                主题
                <Input
                  value={draft.topic}
                  onChange={e => setDraft({ ...draft, topic: e.target.value })}
                />
              </label>
            </div>
            <label>
              标签
              <div className="tag-editor">
                {draft.tags.map(t => (
                  <Tag
                    key={t}
                    onRemove={() =>
                      setDraft({ ...draft, tags: draft.tags.filter(x => x !== t) })
                    }
                  >
                    {t}
                  </Tag>
                ))}
                <button
                  onClick={() =>
                    setDraft({ ...draft, tags: [...draft.tags, '新标签'] })
                  }
                >
                  <Plus />
                  添加
                </button>
              </div>
            </label>
            <div className="form-actions">
              <Button
                variant="ghost"
                onClick={() => {
                  setDraft(item);
                  setEditing(false);
                }}
              >
                取消
              </Button>
              <Button variant="primary" onClick={save}>
                <Save />
                保存更改
              </Button>
            </div>
          </div>
        ) : (
          <>
            <h1>{item.question}</h1>
            <div className="tag-row">
              {item.tags.map(t => (
                <Tag key={t}>{t}</Tag>
              ))}
              <button aria-label="添加标签">
                <Plus />
              </button>
            </div>
            <div className="answer">
              <h2>1. 核心回答</h2>
              <div className="lead">{item.answer.split('\n')[0]}</div>
              {renderAnswer(item.answer.split('\n').slice(1).join('\n'))}
              <hr />
              <h2>2. 可能的追问</h2>
              <ul>
                {item.followUps.map(x => (
                  <li key={x}>{x}</li>
                ))}
              </ul>
            </div>
          </>
        )}
      </article>

      <aside className="detail-rail">
        <DetailRail item={item} related={related} />
      </aside>

      <Drawer
        open={railOpen}
        onClose={() => setRailOpen(false)}
        title="详情"
        side="right"
      >
        <DetailRail item={item} related={related} />
      </Drawer>
    </div>
  );
}
