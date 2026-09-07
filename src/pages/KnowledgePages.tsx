import { useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import ReactMarkdown from 'react-markdown';
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
  Trash2,
  Plus,
  Save,
  Tags,
  FolderOpen,
  Clock,
  Filter,
  Download,
  LoaderCircle,
  Check,
} from '../components/Icons';
import { Button, Input, Tag, Textarea, Empty, Spinner, Confirm } from '../components/UI';
import { Drawer } from '../components/Drawer';
import { useMediaQuery } from '../hooks/useMediaQuery';

const renderAnswer = (text: string) => {
  // Raw HTML is explicitly skipped. The allowlist keeps stored/model-created
  // content inside the compact vocabulary supported by Knowledge Detail.
  return (
    <ReactMarkdown
      skipHtml
      unwrapDisallowed
      allowedElements={['h2', 'h3', 'p', 'strong', 'em', 'code', 'pre', 'ol', 'ul', 'li', 'blockquote', 'br']}
      components={{
        p: props => <p {...props} className="answer-body-text" />,
        li: props => <li {...props} className="answer-body-text" />,
      }}
    >
      {text}
    </ReactMarkdown>
  );
};

const NO_TOPIC = '__no_topic__';

// 主题/领域筛选面板的内容。横屏与桌面仍放在常驻左栏；竖屏放进左侧抽屉，
// 两份渲染共用同一份数据与交互，不做 CSS 隐藏式的功能阉割。
function TopicPanelBody({
  topics,
  knowledge,
  selectedTopic,
  selectedKeyword,
  onPickTopic,
  onPickKeyword,
  onOpenKnowledge,
  onCreateTopic,
}: {
  topics: string[];
  knowledge: Knowledge[];
  selectedTopic: string;
  selectedKeyword: string;
  onPickTopic: (t: string) => void;
  onPickKeyword: (tag: string) => void;
  onOpenKnowledge: (id: string) => void;
  onCreateTopic: () => void;
}) {
  const [mode, setMode] = useState<'topic' | 'keyword'>('topic');
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const keywords = [...new Set(knowledge.flatMap(item => item.tags))].sort((a, b) =>
    a.localeCompare(b, 'zh-CN')
  );
  const toggleTopic = (topic: string) => {
    setExpanded(current => {
      const next = new Set(current);
      if (next.has(topic)) next.delete(topic);
      else next.add(topic);
      return next;
    });
    onPickTopic(topic);
  };
  return (
    <div className="topic-panel-body">
      <div className="tabs">
        <button className={mode === 'topic' ? 'active' : ''} onClick={() => setMode('topic')}>按主题</button>
        <button className={mode === 'keyword' ? 'active' : ''} onClick={() => setMode('keyword')}>按关键词</button>
      </div>
      <div className="domain-list">
        {mode === 'topic' ? [NO_TOPIC, ...topics].map(topic => {
          const noTopic = topic === NO_TOPIC;
          const children = knowledge.filter(item => noTopic ? !item.topic : item.topic === topic);
          const open = expanded.has(topic);
          return <div className="topic-group" key={topic}>
            <button className={selectedTopic === topic ? 'active topic-toggle' : 'topic-toggle'} onClick={() => toggleTopic(topic)} aria-expanded={open}>
              <ChevronRight className={open ? 'expanded' : ''}/>{noTopic ? '无主题' : topic}<small>{children.length}</small>
            </button>
            {open && <div className="topic-children">{children.map(item => <button key={item.id} onClick={() => onOpenKnowledge(item.id)}>{item.question}</button>)}</div>}
          </div>;
        }) : keywords.map(keyword => (
          <button className={selectedKeyword === keyword ? 'active keyword' : 'keyword'} key={keyword} onClick={() => onPickKeyword(keyword)}>
            {keyword}<small>{knowledge.filter(item => item.tags.includes(keyword)).length}</small>
          </button>
        ))}
      </div>
      {mode === 'topic' && <Button variant="ghost" onClick={onCreateTopic}>
        <Plus /> 新建主题
      </Button>}
    </div>
  );
}

export function KnowledgePage() {
  const { knowledge, topics, refresh } = useApp();
  const nav = useNavigate();
  const [q, setQ] = useState('');
  const [selectedTopic, setSelectedTopic] = useState('');
  const [selectedKeyword, setSelectedKeyword] = useState('');
  const [filterOpen, setFilterOpen] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [selecting, setSelecting] = useState(false);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [topicDialogOpen, setTopicDialogOpen] = useState(false);
  const [newTopicName, setNewTopicName] = useState('');
  const [topicError, setTopicError] = useState('');
  const [creatingTopic, setCreatingTopic] = useState(false);

  const exportKnowledge = async (ids?: string[]) => {
    setExporting(true);
    try {
      const date = new Date().toISOString().slice(0, 10);
      const kind = ids ? 'selected' : 'all';
      const result = await knowledgeService.exportMarkdown(`interview-kit-knowledge-${kind}-${date}.md`, ids);
      if (result) window.alert(`已导出 ${result.itemCount} 条知识到：\n${result.path}`);
    } catch (error) {
      window.alert(`导出失败：${String(error)}`);
    } finally {
      setExporting(false);
    }
  };

  const toggleSelected = (id: string) => {
    setSelectedIds(current => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  // 切到横屏/桌面双栏时左栏已经常驻，抽屉必须收起，避免同一内容重叠出现。
  const twoPane = useMediaQuery('(min-width: 700px)');
  useEffect(() => {
    if (twoPane) setFilterOpen(false);
  }, [twoPane]);

  const filtered = knowledge.filter(
    x =>
      (!selectedTopic || (selectedTopic === NO_TOPIC ? !x.topic : x.topic === selectedTopic)) &&
      (!selectedKeyword || x.tags.includes(selectedKeyword)) &&
      [x.question, x.answer, x.topic, ...x.tags]
        .join(' ')
        .toLowerCase()
        .includes(q.toLowerCase())
  );

  const pickTopic = (topic: string) => {
    setSelectedTopic(current => current === topic ? '' : topic);
    setSelectedKeyword('');
  };
  const pickKeyword = (keyword: string) => {
    setSelectedKeyword(current => current === keyword ? '' : keyword);
    setSelectedTopic('');
  };
  const createTopic = async () => {
    const name = newTopicName.trim();
    if (!name) {
      setTopicError('请输入主题名称。');
      return;
    }
    if (topics.includes(name)) {
      setTopicError('这个主题已经存在。');
      return;
    }
    setCreatingTopic(true);
    setTopicError('');
    try {
      await knowledgeService.createTopic(name);
      await refresh();
      setSelectedTopic(name);
      setSelectedKeyword('');
      setTopicDialogOpen(false);
      setNewTopicName('');
    } catch (error) {
      setTopicError(String(error));
    } finally {
      setCreatingTopic(false);
    }
  };
  const openTopicDialog = () => {
    setNewTopicName('');
    setTopicError('');
    setTopicDialogOpen(true);
  };

  return (
    <div className="knowledge-browser">
      <aside className="topic-panel">
        <h1>知识库</h1>
        <TopicPanelBody
          topics={topics}
          knowledge={knowledge}
          selectedTopic={selectedTopic}
          selectedKeyword={selectedKeyword}
          onPickTopic={pickTopic}
          onPickKeyword={pickKeyword}
          onOpenKnowledge={id => nav(`/knowledge/${id}`)}
          onCreateTopic={openTopicDialog}
        />
      </aside>

      <section className="knowledge-list">
        <header>
          <div>
            <p>{selectedTopic === NO_TOPIC ? '无主题' : selectedTopic || selectedKeyword || '全部'}</p>
            <h2>{q ? `“${q}” 的搜索结果` : selectedTopic ? `${selectedTopic === NO_TOPIC ? '无主题' : selectedTopic}知识` : selectedKeyword ? `${selectedKeyword} 关键词` : '全部知识'}</h2>
          </div>
          <div className="knowledge-header-actions">
            {selecting ? (
              <>
                <Button
                  variant="ghost"
                  onClick={() => {
                    const visibleIds = filtered.map(item => item.id);
                    const allSelected = visibleIds.every(id => selectedIds.has(id));
                    setSelectedIds(current => {
                      const next = new Set(current);
                      visibleIds.forEach(id => allSelected ? next.delete(id) : next.add(id));
                      return next;
                    });
                  }}
                  disabled={filtered.length === 0}
                >
                  全选当前结果
                </Button>
                <Button
                  variant="primary"
                  onClick={() => exportKnowledge([...selectedIds])}
                  disabled={exporting || selectedIds.size === 0}
                >
                  {exporting ? <LoaderCircle className="spin" /> : <Download />}
                  导出选中（{selectedIds.size}）
                </Button>
                <Button variant="ghost" onClick={() => { setSelecting(false); setSelectedIds(new Set()); }}>
                  取消
                </Button>
              </>
            ) : (
              <>
                <Button onClick={() => setSelecting(true)} disabled={knowledge.length === 0}>多选</Button>
                <Button onClick={() => exportKnowledge()} disabled={exporting || knowledge.length === 0}>
                  {exporting ? <LoaderCircle className="spin" /> : <Download />}
                  导出全部
                </Button>
              </>
            )}
            <div className="small-search">
              <Search />
              <input
                placeholder="搜索知识库…"
                value={q}
                onChange={e => setQ(e.target.value)}
              />
            </div>
          </div>
        </header>

        {/* 竖屏筛选工具条：常驻左栏被 CSS 隐藏后，这里是唯一的筛选入口 */}
        <div className="knowledge-filter-bar">
          <span className="filter-current">
            当前：{selectedTopic ? `主题 ${selectedTopic === NO_TOPIC ? '无主题' : selectedTopic}` : selectedKeyword ? `关键词 ${selectedKeyword}` : '全部'}
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
            aria-label="选择主题或关键词"
          >
            <Filter /> 选择主题 / 关键词
          </button>
        </div>

        {filtered.length ? (
          <div className="knowledge-rows">
            {filtered.map(x => (
              <button
                key={x.id}
                className={selectedIds.has(x.id) ? 'selected' : ''}
                aria-pressed={selecting ? selectedIds.has(x.id) : undefined}
                onClick={() => selecting ? toggleSelected(x.id) : nav(`/knowledge/${x.id}`)}
              >
                {selecting && selectedIds.has(x.id) ? <Check className="selection-check" /> : <FileText />}
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
                    {x.topic || '无主题'}
                  </small>
                  {selecting ? <span>{selectedIds.has(x.id) ? '已选择' : '点击选择'}</span> : <ChevronRight />}
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
        title="筛选主题 / 关键词"
        side="left"
      >
        <TopicPanelBody
          topics={topics}
          knowledge={knowledge}
          selectedTopic={selectedTopic}
          selectedKeyword={selectedKeyword}
          onPickTopic={t => {
            pickTopic(t);
            setFilterOpen(false);
          }}
          onPickKeyword={keyword => {
            pickKeyword(keyword);
            setFilterOpen(false);
          }}
          onOpenKnowledge={id => nav(`/knowledge/${id}`)}
          onCreateTopic={openTopicDialog}
        />
      </Drawer>
      {topicDialogOpen && <div className="restore-overlay" role="dialog" aria-modal="true" aria-labelledby="new-topic-title" onClick={() => !creatingTopic && setTopicDialogOpen(false)}>
        <form className="restore-card topic-create-card" onSubmit={e => {e.preventDefault();void createTopic()}} onClick={e => e.stopPropagation()}>
          <div className="topic-create-heading"><span><FolderOpen /></span><div><h3 id="new-topic-title">新建主题</h3><p>创建一个由你维护的知识分类。AI 只能从已有主题中推荐。</p></div></div>
          <label htmlFor="new-topic-name">主题名称</label>
          <Input id="new-topic-name" autoFocus maxLength={24} placeholder="例如：Redis、MySQL、计算机网络" value={newTopicName} onChange={e => {setNewTopicName(e.target.value);setTopicError('')}} aria-invalid={Boolean(topicError)} />
          <div className="topic-create-hint"><span>{topicError || '最多 24 个字符'}</span><small>{newTopicName.trim().length} / 24</small></div>
          <div className="restore-actions"><Button type="button" variant="ghost" disabled={creatingTopic} onClick={() => setTopicDialogOpen(false)}>取消</Button><Button type="submit" variant="primary" disabled={creatingTopic || !newTopicName.trim()}>{creatingTopic ? '创建中…' : '创建主题'}</Button></div>
        </form>
      </div>}
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
          <FolderOpen /> {item.topic || '无主题'}
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
  const { knowledge, topics, refresh } = useApp();
  const nav = useNavigate();
  const [item, setItem] = useState<Knowledge>();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Knowledge>();
  const [railOpen, setRailOpen] = useState(false);
  const [moreOpen, setMoreOpen] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const moreRef = useRef<HTMLDivElement>(null);

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

  // Click outside (or ESC) closes the "more" popover so the menu doesn't
  // linger after the user moves on.
  useEffect(() => {
    if (!moreOpen) return;
    const onDown = (e: MouseEvent) => {
      if (moreRef.current && !moreRef.current.contains(e.target as Node)) {
        setMoreOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMoreOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [moreOpen]);

  const related = useMemo(
    () =>
      item
        ? knowledge
            .filter(
              x =>
                item.relatedIds.includes(x.id) ||
                (Boolean(item.topic) && x.topic === item.topic && x.id !== item.id)
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

  const confirmDeleteItem = async () => {
    if (deleting) return;
    setDeleting(true);
    try {
      await knowledgeService.delete(item.id);
      await refresh();
      setMoreOpen(false);
      setConfirmDelete(false);
      nav('/knowledge');
    } finally {
      setDeleting(false);
    }
  };

  return (
    <div className="detail">
      <div className="detail-top">
        <button onClick={() => nav('/knowledge')}>
          <ChevronLeft /> 知识库
        </button>
        <span>{item.topic || '无主题'}</span>
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
          <div className="more-wrap" ref={moreRef}>
            <button
              aria-label="更多操作"
              aria-expanded={moreOpen}
              aria-haspopup="menu"
              onClick={() => setMoreOpen(v => !v)}
              className={moreOpen ? 'active' : ''}
            >
              <MoreHorizontal />
            </button>
            {moreOpen && (
              <div className="more-menu" role="menu">
                <button
                  role="menuitem"
                  className="danger"
                  onClick={() => {
                    setMoreOpen(false);
                    setConfirmDelete(true);
                  }}
                >
                  <Trash2 /> 删除这条知识
                </button>
              </div>
            )}
          </div>
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
              答案
              <Textarea
                rows={16}
                value={draft.answer}
                onChange={e => setDraft({ ...draft, answer: e.target.value })}
              />
            </label>
            <label>
              主题
              <select className="input" value={draft.topic} onChange={e => {
                const topic = e.target.value;
                const existing = knowledge.find(entry => entry.topic === topic);
                setDraft({ ...draft, topic, domain: existing?.domain || '未分类' });
              }}>
                <option value="">无主题</option>
                {topics.map(topic => <option key={topic} value={topic}>{topic}</option>)}
              </select>
            </label>
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
              {renderAnswer(item.answer)}
              {item.followUps.length > 0 && (
                <section className="follow-up-section" aria-labelledby="follow-up-title">
                  <h2 id="follow-up-title" className="follow-up-title">可能的追问</h2>
                  <ul>
                    {item.followUps.map(x => (
                      <li key={x}>{x}</li>
                    ))}
                  </ul>
                </section>
              )}
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

      <Confirm
        open={confirmDelete}
        title="删除这条知识？"
        body={
          <>
            这条知识将从知识库中移除，列表与搜索中不再出现。
            <br />
            <small>当前没有自动同步，本次删除不会传播到其他设备；如需恢复请在下次手动备份前操作。</small>
          </>
        }
        confirmText={deleting ? '删除中…' : '删除'}
        cancelText="取消"
        destructive
        onCancel={() => !deleting && setConfirmDelete(false)}
        onConfirm={confirmDeleteItem}
      />
    </div>
  );
}
