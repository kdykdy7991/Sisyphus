import { useEffect, useId, useRef, type ReactNode } from 'react';
import { X } from './Icons';

const FOCUSABLE =
  'a[href],button:not([disabled]),textarea:not([disabled]),input:not([disabled]),select:not([disabled]),[tabindex]:not([tabindex="-1"])';

type DrawerProps = {
  open: boolean;
  onClose: () => void;
  title: string;
  side?: 'left' | 'right';
  children: ReactNode;
};

/**
 * 竖屏专用抽屉。关闭方式：关闭按钮、点击遮罩、ESC、Android 返回键。
 *
 * Android 返回键通过 history 实现：Tauri 生成的 WryActivity 在
 * `mWebView.canGoBack()` 为真时会调用 `goBack()`，所以打开时压一条历史记录，
 * 返回键落到 popstate 上即可关闭；通过 UI 关闭时主动 `history.back()` 弹出，
 * 避免历史栈里留下空记录。
 */
export function Drawer({
  open,
  onClose,
  title,
  side = 'right',
  children,
}: DrawerProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const restoreRef = useRef<HTMLElement | null>(null);
  const pushedRef = useRef(false);
  const titleId = useId();

  // 打开：记录触发元素、压历史、锁背景滚动；关闭：弹历史、还原焦点。
  useEffect(() => {
    if (!open) return;

    restoreRef.current = document.activeElement as HTMLElement | null;
    window.history.pushState({ padDrawer: title }, '');
    pushedRef.current = true;
    document.body.classList.add('drawer-open');

    const onPopState = () => {
      pushedRef.current = false;
      onClose();
    };
    window.addEventListener('popstate', onPopState);

    return () => {
      window.removeEventListener('popstate', onPopState);
      if (pushedRef.current) {
        pushedRef.current = false;
        window.history.back();
      }
      document.body.classList.remove('drawer-open');
      const el = restoreRef.current;
      restoreRef.current = null;
      if (el && document.contains(el)) el.focus();
    };
  }, [open, onClose, title]);

  // ESC 关闭 + Tab 焦点循环。
  useEffect(() => {
    if (!open) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose();
        return;
      }
      if (e.key !== 'Tab') return;
      const panel = panelRef.current;
      if (!panel) return;
      const items = Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE));
      if (items.length === 0) return;
      const first = items[0];
      const last = items[items.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', onKeyDown, true);
    return () => document.removeEventListener('keydown', onKeyDown, true);
  }, [open, onClose]);

  // 打开后焦点进入抽屉（关闭按钮）。
  useEffect(() => {
    if (!open) return;
    const id = window.setTimeout(() => {
      const btn = panelRef.current?.querySelector<HTMLElement>(
        '[data-drawer-close]'
      );
      (btn ?? panelRef.current)?.focus();
    }, 0);
    return () => window.clearTimeout(id);
  }, [open]);

  if (!open) return null;

  return (
    <>
      <div className="drawer-backdrop" onClick={onClose} />
      <div
        className={`drawer is-${side}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        ref={panelRef}
        tabIndex={-1}
      >
        <div className="drawer-head">
          <h2 id={titleId}>{title}</h2>
          <button data-drawer-close onClick={onClose} aria-label="关闭">
            <X aria-hidden="true" />
          </button>
        </div>
        <div className="drawer-body">{children}</div>
      </div>
    </>
  );
}
