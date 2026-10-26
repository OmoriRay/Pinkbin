import { useEffect, useState } from 'react';
import { Clock3 } from 'lucide-react';
import type { ChatTurn } from '../store';

export function ReplyTiming({ turn }: { turn: ChatTurn }) {
  const [now, setNow] = useState(() => performance.now());
  const { pending, requestStartedAt, durationMs, failed } = turn;
  useEffect(() => {
    if (!pending || requestStartedAt === undefined) return;
    const timer = window.setInterval(() => setNow(performance.now()), 250);
    return () => window.clearInterval(timer);
  }, [pending, requestStartedAt]);

  if (requestStartedAt === undefined) return null;
  const elapsed = pending ? Math.max(0, now - requestStartedAt) : durationMs;
  if (elapsed === undefined) return null;
  const label = pending ? '已等待' : failed ? '耗时' : '用时';
  return (
    <div className="chat-reply-timing" title="从发起请求到收到完整回复的时间，包含网络等待">
      <Clock3 size={12} aria-hidden="true" />
      <span>{label} {(elapsed / 1000).toFixed(1)} 秒</span>
    </div>
  );
}
