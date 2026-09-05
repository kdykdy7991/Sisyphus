import { NavLink, Outlet } from 'react-router-dom';
import { BookOpen, Home, MessageCircle, ImagePlus, Settings } from './Icons';

// 侧边栏导航。设计稿只画了首页 / 导入图片 / 对话 / 设置 4 项，但首页
// 各区块的「查看全部」以及搜索结果都跳转 /knowledge，入口必须保留。
// 横屏（≥700px）使用左侧紧凑图标侧栏；竖屏（≤699px）使用同一份数据
// 渲染的底部导航；CSS 控制可见性，组件本身不做媒体判断。
const nav = [
  ['/', '首页', Home, 'home'],
  ['/knowledge', '知识库', BookOpen, 'knowledge'],
  ['/import', '导入图片', ImagePlus, 'import'],
  ['/chat', '对话', MessageCircle, 'chat'],
  ['/settings', '设置', Settings, 'settings'],
] as const;

export function Shell() {
  return (
    <div className="app-shell">
      <aside className="sidebar" aria-label="主导航">
        <div className="brand">
          <BookOpen />
          <div>
            <b>Interview Kit</b>
            <small>面试知识工作台</small>
          </div>
        </div>
        <nav>
          {nav.map(([to, label, Icon, id]) => (
            <NavLink key={to} to={to} end={to === '/'} aria-label={label}>
              <Icon aria-hidden="true" />
              <span>{label}</span>
            </NavLink>
          ))}
        </nav>
        <blockquote>
          Better Preparation
          <br />
          A Brighter You
        </blockquote>
      </aside>
      <main className="main">
        <Outlet />
      </main>
      {/* 竖屏底部导航：与侧栏共用同一份数据源；CSS 在 ≤899px 且无 hover 时显示 */}
      <nav className="bottom-nav" aria-label="主导航（底部）">
        {nav.map(([to, label, Icon, id]) => (
          <NavLink key={to} to={to} end={to === '/'} aria-label={label}>
            <Icon aria-hidden="true" />
            <span>{label}</span>
          </NavLink>
        ))}
      </nav>
    </div>
  );
}
