import { useEffect, useState } from 'react';

/**
 * 订阅 matchMedia。只有"同一时刻显示哪个面板"这类状态性判断才需要它，
 * 纯展示差异仍然交给 CSS 断点处理。监听在卸载时正确移除。
 */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(
    () => typeof window !== 'undefined' && window.matchMedia(query).matches
  );

  useEffect(() => {
    const mql = window.matchMedia(query);
    const onChange = (e: MediaQueryListEvent) => setMatches(e.matches);
    setMatches(mql.matches);
    mql.addEventListener('change', onChange);
    return () => mql.removeEventListener('change', onChange);
  }, [query]);

  return matches;
}
