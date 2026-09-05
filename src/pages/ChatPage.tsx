import { useEffect, useRef, useState, type CSSProperties } from 'react';
import { useNavigate } from 'react-router-dom';
import { useApp } from '../AppContext';
import { chatService } from '../services';
import type { ChatMessage, Knowledge } from '../types';
import {
  BookOpen,
  History,
  MoreHorizontal,
  ArrowUp,
  FileText,
  Copy,
  ThumbsUp,
  ThumbsDown,
  LoaderCircle,
  ChevronRight,
} from '../components/Icons';
import { Drawer } from '../components/Drawer';
import { useMediaQuery } from '../hooks/useMediaQuery';

const initial: ChatMessage[] = [
  {
    id: 'welcome',
    role: 'assistant',
    content: '你可以向自己的知识库提问。我会综合已有内容回答，并标出参考知识。',
  },
];

// 右栏内容。横屏/桌面常驻显示，竖屏进入"参考知识"抽屉。
function ChatRail({
  refs,
  knowledge,
  onAsk,
}: {
  refs: Knowledge[];
  knowledge: Knowledge[];
  onAsk: (q: string) => void;
}) {
  const nav = useNavigate();
  return (
    <div className="rail-body">
      <section>
        <h3>
          参考知识 <small>{refs.length}</small>
        </h3>
        <p>这些内容来自你的知识库</p>
        {refs.map(x => (
          <button key={x.id} onClick={() => nav(`/knowledge/${x.id}`)}>
            <FileText />
            <span>
              <b>{x.question}</b>
              <small>
                {x.domain} / {x.topic}
              </small>
            </span>
            <ChevronRight />
          </button>
        ))}
      </section>
      <section>
        <h3>相关问题</h3>
        {knowledge.slice(1, 6).map(x => (
          <button key={x.id} onClick={() => onAsk(x.question)}>
            <FileText />
            <span>{x.question}</span>
          </button>
        ))}
      </section>
      <blockquote>
        “
        <br />
        重复的问题，
        <br />
        值得用更好的答案再写一遍。　”
      </blockquote>
    </div>
  );
}

export function ChatPage() {
  const { knowledge } = useApp();
  const nav = useNavigate();
  const [messages, setMessages] = useState(initial);
  const [value, setValue] = useState('');
  const [scope, setScope] = useState('');
  const [loading, setLoading] = useState(false);
  const [refsOpen, setRefsOpen] = useState(false);
  const [kbInset, setKbInset] = useState(0);

  const messagesRef = useRef<HTMLDivElement>(null);

  const twoPane = useMediaQuery('(min-width: 700px)');
  useEffect(() => {
    if (twoPane) setRefsOpen(false);
  }, [twoPane]);

  // 软键盘：Android WebView 在 pan 模式下布局视口不缩小，这里用 visualViewport
  // 计算被遮挡的高度并下压对话容器。index.html 的
  // `interactive-widget=resizes-content` 生效时该值自然为 0，两者不会叠加。
  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    const update = () => {
      const overlap = Math.max(
        0,
        window.innerHeight - (vv.height + vv.offsetTop)
      );
      setKbInset(overlap > 24 ? overlap : 0);
    };
    update();
    vv.addEventListener('resize', update);
    vv.addEventListener('scroll', update);
    window.addEventListener('resize', update);
    return () => {
      vv.removeEventListener('resize', update);
      vv.removeEventListener('scroll', update);
      window.removeEventListener('resize', update);
    };
  }, []);

  // 键盘弹出/收起或收到新消息时，把最新内容滚进可见区域。
  useEffect(() => {
    const el = messagesRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [kbInset, messages.length, loading]);

  const domains = [...new Set(knowledge.map(x => x.domain).filter(Boolean))] as string[];
  const topics = [...new Set(knowledge.map(x => x.topic).filter(Boolean))] as string[];
  const scopeName = scope === '' ? '全部知识' : scope.slice(scope.indexOf(':') + 1);
  const latest = [...messages].reverse().find(x => x.role === 'assistant' && x.citations?.length);
  const refs: Knowledge[] =
    latest?.citations
      ?.map(id => knowledge.find(x => x.id === id))
      .filter((x): x is Knowledge => Boolean(x)) || knowledge.slice(0, 3);

  const ask = async () => {
    if (!value.trim() || loading) return;
    const q: ChatMessage = { id: `q-${Date.now()}`, role: 'user', content: value };
    setMessages(x => [...x, q]);
    setValue('');
    setLoading(true);
    try {
      const a = await chatService.ask(q.content, scope, [...messages, q]);
      setMessages(x => [...x, a]);
    } catch (e) {
      setMessages(x => [
        ...x,
        { id: `e-${Date.now()}`, role: 'assistant', content: `出错了：${String(e)}` },
      ]);
    }
    setLoading(false);
  };

  return (
    <div
      className="chat"
      style={{ '--kb-inset': `${kbInset}px` } as CSSProperties}
    >
      <section className="conversation">
        <header>
          <select value={scope} onChange={e => setScope(e.target.value)}>
            <option value="">全部知识</option>
            {domains.map(d => (
              <option key={'d' + d} value={`domain:${d}`}>
                {d}
              </option>
            ))}
            {topics.map(t => (
              <option key={'t' + t} value={`topic:${t}`}>
                {t}
              </option>
            ))}
          </select>
          <span>基于你的知识库回答问题</span>
          {/* 竖屏参考知识入口：右栏被 CSS 隐藏后，参考知识与相关问题都从这里进 */}
          <button
            className="refs-toggle"
            onClick={() => setRefsOpen(true)}
            aria-label="查看参考知识"
          >
            <BookOpen /> 参考知识
          </button>
          <button className="history-btn" aria-label="历史对话">
            <History />
            <span>历史对话</span>
          </button>
          <button aria-label="更多操作">
            <MoreHorizontal />
          </button>
        </header>

        <div className="messages" ref={messagesRef}>
          {messages.map(m =>
            m.role === 'user' ? (
              <div className="user-message" key={m.id}>
                {m.content}
                <small>刚刚</small>
              </div>
            ) : (
              <div className="assistant-message" key={m.id}>
                <div className="ai-mark">IK</div>
                <article>
                  <small>基于你的知识库 · 范围：{scopeName}</small>
                  {m.content.split('\n').map((p, i) => (
                    <p key={i}>{p}</p>
                  ))}
                  {m.citations && (
                    <div className="inline-citations">
                      {m.citations.map((id, i) => {
                        const k = knowledge.find(x => x.id === id);
                        return k ? (
                          <button key={id} onClick={() => nav(`/knowledge/${id}`)}>
                            [{i + 1}] {k.question}
                          </button>
                        ) : null;
                      })}
                    </div>
                  )}
                  <div className="message-tools">
                    <Copy />
                    <ThumbsUp />
                    <ThumbsDown />
                  </div>
                </article>
              </div>
            )
          )}
          {loading && (
            <div className="assistant-message loading">
              <div className="ai-mark">IK</div>
              <span>
                <LoaderCircle />
                正在检索知识并组织回答…
              </span>
            </div>
          )}
        </div>

        <div className="composer">
          <textarea
            value={value}
            onChange={e => setValue(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                ask();
              }
            }}
            placeholder="继续提问，或输入新的问题…"
          />
          <div>
            <span>
              <BookOpen /> 知识库范围：{scopeName}
            </span>
            <small className="enter-hint">Enter 发送</small>
            <button
              className="send"
              onClick={ask}
              disabled={!value.trim() || loading}
              title="发送（Enter）"
              aria-label="发送"
            >
              {loading ? <LoaderCircle className="spin" /> : <ArrowUp />}
            </button>
          </div>
        </div>
      </section>

      <aside className="chat-rail">
        <ChatRail refs={refs} knowledge={knowledge} onAsk={setValue} />
      </aside>

      <Drawer
        open={refsOpen}
        onClose={() => setRefsOpen(false)}
        title="参考知识"
        side="right"
      >
        <ChatRail
          refs={refs}
          knowledge={knowledge}
          onAsk={q => {
            setValue(q);
            setRefsOpen(false);
          }}
        />
      </Drawer>
    </div>
  );
}
