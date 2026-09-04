import { NavLink, Outlet } from 'react-router-dom';
import { BookOpen, Home, MessageCircle, ImagePlus, Settings } from './Icons';

// 侧边栏导航。设计稿只画了首页 / 导入图片 / 对话 / 设置 4 项，但首页
// 各区块的「查看全部」以及搜索结果都跳转 /knowledge，入口必须保留。
const nav = [
  ['/', '首页', Home],
  ['/knowledge', '知识库', BookOpen],
  ['/import', '导入图片', ImagePlus],
  ['/chat', '对话', MessageCircle],
  ['/settings', '设置', Settings],
] as const;

export function Shell() {
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <BookOpen />
          <div>
            <b>Interview Kit</b>
          </div>
        </div>
        <nav>
          {nav.map(([to, label, Icon]) => (
            <NavLink key={to} to={to} end={to === '/'}>
              <Icon />
              {label}
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
    </div>
  );
}