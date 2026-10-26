import { useEffect, useRef, useState } from 'react';
import { Send, Trash2, ShieldCheck, ShieldAlert, ShieldX, X, MessageSquare, Sparkles, Folder, File, ImagePlus } from 'lucide-react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { api } from '../api';
import { isTauri } from '../env';
import { useStore } from '../store';
import { formatBytes } from '../format';
import { freeChat, overviewChat } from '../advisorClient';
import type { Node, AdvisorResponse } from '../types';
import { ReplyTiming } from './ReplyTiming';

function uid() {
  return Math.random().toString(36).slice(2);
}

function findNodeByPath(root: Node | null, path: string): Node | null {
  if (!root) return null;
  if (root.path === path) return root;
  for (const c of root.children) {
    const f = findNodeByPath(c, path);
    if (f) return f;
  }
  return null;
}

function buildOverviewSummary(root: Node) {
  const flatten = (n: Node, depth: number, out: { path: string; name: string; size: number; depth: number; is_dir: boolean }[]) => {
    if (depth > 0) {
      out.push({ path: n.path, name: n.name, size: n.size, depth, is_dir: n.is_dir });
    }
    if (depth < 2) {
      for (const c of n.children ?? []) flatten(c, depth + 1, out);
    }
  };
  const all: { path: string; name: string; size: number; depth: number; is_dir: boolean }[] = [];
  flatten(root, 0, all);
  all.sort((a, b) => b.size - a.size);
  const top = all.slice(0, 25).map((x) => ({
    path: x.path,
    name: x.name,
    size_human: formatBytes(x.size),
    size_bytes: x.size,
    depth: x.depth,
    kind: x.is_dir ? 'dir' : 'file',
  }));
  return {
    root: root.path,
    total_size_human: formatBytes(root.size),
    total_files: root.file_count,
    top_entries: top,
  };
}

export function ChatPanel() {
  const root = useStore((s) => s.root);
  const chat = useStore((s) => s.chat);
  const pushTurn = useStore((s) => s.pushChatTurn);
  const patchTurn = useStore((s) => s.patchChatTurn);
  const setBusy = useStore((s) => s.setChatBusy);
  const resetChat = useStore((s) => s.resetChat);
  const addReclaimed = useStore((s) => s.addReclaimed);

  const [input, setInput] = useState('');
  const [pendingDrops, setPendingDrops] = useState<{ path: string; name: string }[]>([]);
  const [pendingImages, setPendingImages] = useState<{ id: string; name: string; dataUrl: string; mimeType: string }[]>([]);
  const [dropping, setDropping] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const overviewFiredFor = useRef<Node | null>(null);
  const requestInFlight = useRef(false);

  const node = chat.node;

  // Wait for an ongoing conversation request, then summarize each scan once.
  useEffect(() => {
    if (!root || chat.busy || requestInFlight.current) return;
    if (overviewFiredFor.current === root) return;
    overviewFiredFor.current = root;
    void runOverview(root);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [root, chat.busy]);

  // No more auto-advice on node change — the user runs ONE conversation,
  // dropped items become pending pills until they send.

  useEffect(() => {
    scrollerRef.current?.scrollTo({ top: scrollerRef.current.scrollHeight, behavior: 'smooth' });
  }, [chat.turns.length, chat.busy]);

  const runOverview = async (r: Node) => {
    if (requestInFlight.current) return;
    requestInFlight.current = true;
    const requestStartedAt = performance.now();
    setBusy(true);
    const turnId = uid();
    pushTurn({
      id: turnId,
      role: 'assistant',
      text: `已扫完 ${r.path} · ${formatBytes(r.size)} · ${r.file_count.toLocaleString()} 个文件。AI 正在生成整体解析…`,
      pending: true,
      requestStartedAt,
    });
    try {
      const summary = buildOverviewSummary(r);
      const reply = await overviewChat(summary);
      patchTurn(turnId, { text: reply, pending: false, durationMs: performance.now() - requestStartedAt });
    } catch (e) {
      patchTurn(turnId, {
        text: `（AI 总览失败：${String(e)}）\n你可以从左边把任意文件夹/文件拖进来问。`,
        pending: false,
        failed: true,
        durationMs: performance.now() - requestStartedAt,
      });
    } finally {
      requestInFlight.current = false;
      setBusy(false);
    }
  };

  const askFollowUp = async () => {
    if (chat.busy || requestInFlight.current) return;
    const text = input.trim();
    if (!text && pendingDrops.length === 0 && pendingImages.length === 0) return;
    requestInFlight.current = true;
    const requestStartedAt = performance.now();
    setInput('');
    const drops = pendingDrops.slice();
    const images = pendingImages.slice();
    setPendingDrops([]);
    setPendingImages([]);

    const dropDesc = drops.length > 0 ? `（关于：${drops.map((d) => d.path).join('、')}）` : '';
    const imgDesc = images.length > 0 ? `（带 ${images.length} 张图片）` : '';
    const userText = [text, dropDesc, imgDesc].filter(Boolean).join('\n');
    pushTurn({ id: uid(), role: 'user', text: userText });
    setBusy(true);
    const turnId = uid();
    pushTurn({ id: turnId, role: 'assistant', text: '思考中…', pending: true, requestStartedAt });

    try {
      const targets = drops.length > 0 && root
        ? drops.map((d) => findNodeByPath(root, d.path)).filter(Boolean) as Node[]
        : node ? [node] : [];

      const ctx = targets.map((t) => ({
        path: t.path,
        name: t.name,
        size: formatBytes(t.size),
        is_dir: t.is_dir,
        file_count: t.file_count,
        top_extensions: (t.top_extensions ?? []).slice(0, 6),
        sample_children: (t.children ?? []).slice(0, 8).map((c) => ({ name: c.name, size: formatBytes(c.size), is_dir: c.is_dir })),
      }));
      const recentConversation = chat.turns
        .filter((turn) => (turn.role === 'user' || turn.role === 'assistant') && !turn.pending && !turn.failed)
        .slice(-6)
        .map((turn) => ({ role: turn.role, text: turn.text.slice(0, 1000) }));
      const scanContext = root
        ? `当前扫描状态：已扫描。以下是最近一次扫描的快照，不是实时文件状态：${JSON.stringify({ path: root.path, size: formatBytes(root.size), file_count: root.file_count })}`
        : '当前扫描状态：未扫描。尚未获取本机磁盘或文件数据，可正常聊天；用户描述的路径和体积尚未核实。';
      const contextLine = [
        scanContext,
        ctx.length > 0 ? `目标对象：${JSON.stringify(ctx, null, 2)}` : '',
        recentConversation.length > 0 ? `近期文字对话（仅供理解前文，不是扫描证据）：${JSON.stringify(recentConversation)}` : '',
      ].filter(Boolean).join('\n\n');
      const reply = await freeChat(
        contextLine,
        text || (images.length > 0 ? '看看这张图，告诉我是什么、能不能删。' : '这些是什么？能不能删？'),
        images.length > 0 ? images.map((i) => ({ dataUrl: i.dataUrl, mimeType: i.mimeType })) : undefined,
      );
      patchTurn(turnId, { text: reply, pending: false, durationMs: performance.now() - requestStartedAt });
    } catch (e) {
      patchTurn(turnId, { text: `AI 调用失败：${String(e)}`, pending: false, failed: true, durationMs: performance.now() - requestStartedAt });
    } finally {
      requestInFlight.current = false;
      setBusy(false);
    }
  };

  const fileToDataUrl = (file: File) =>
    new Promise<string>((resolve, reject) => {
      const r = new FileReader();
      r.onerror = () => reject(r.error);
      r.onload = () => resolve(r.result as string);
      r.readAsDataURL(file);
    });

  const addImageFile = async (file: File) => {
    if (!file.type.startsWith('image/')) return;
    if (file.size > 20 * 1024 * 1024) {
      pushTurn({ id: uid(), role: 'system', text: `图片 ${file.name} 太大（>20MB），跳过` });
      return;
    }
    try {
      const dataUrl = await fileToDataUrl(file);
      setPendingImages((prev) => [
        ...prev,
        { id: uid(), name: file.name || 'image', dataUrl, mimeType: file.type || 'image/png' },
      ]);
    } catch (e) {
      pushTurn({ id: uid(), role: 'system', text: `读取图片失败：${String(e)}` });
    }
  };

  const onPaste = async (e: React.ClipboardEvent) => {
    const items = e.clipboardData?.items;
    if (!items) return;
    for (const it of items) {
      if (it.kind === 'file') {
        const f = it.getAsFile();
        if (f && f.type.startsWith('image/')) {
          e.preventDefault();
          await addImageFile(f);
        }
      }
    }
  };

  const onDrop = async (e: React.DragEvent) => {
    e.preventDefault();
    setDropping(false);
    // Image file drop (from filesystem / browser): take precedence over the
    // internal Pinkbin path drop, since paths come with our custom mime type.
    const files = Array.from(e.dataTransfer.files ?? []).filter((f) => f.type.startsWith('image/'));
    if (files.length > 0) {
      for (const f of files) await addImageFile(f);
      return;
    }
    const path = e.dataTransfer.getData('application/x-pinkbin-path');
    const name = e.dataTransfer.getData('application/x-pinkbin-name') || path.split(/[\\/]/).pop() || path;
    if (!path) return;
    // Just stage a pending pill — do NOT reset the conversation or refocus the
    // chat node. The user keeps one continuous conversation and asks across
    // multiple dropped items.
    setPendingDrops((prev) => (prev.find((p) => p.path === path) ? prev : [...prev, { path, name }]));
  };

  const deletingPaths = useRef(new Set<string>());
  const deleteNode = async (target: Node, reason: string) => {
    if (deletingPaths.current.has(target.path)) return;
    deletingPaths.current.add(target.path);
    try {
      await api.execute({ action: 'delete', paths: [target.path], reason }, false);
      addReclaimed(target.size);
      pushTurn({ id: uid(), role: 'system', text: `已直接删除 ${target.path} · 文件大小 ${formatBytes(target.size)}` });
    } catch (e) {
      deletingPaths.current.delete(target.path);
      pushTurn({ id: uid(), role: 'system', text: `删除失败：${String(e)}` });
    }
  };

  const empty = chat.turns.length === 0;

  return (
    <div
      className={'chat' + (dropping ? ' drop-target' : '')}
      onDragOver={(e) => { e.preventDefault(); setDropping(true); }}
      onDragLeave={() => setDropping(false)}
      onDrop={onDrop}
    >
      <div className="chat-head">
        <Sparkles size={15} />
        <div style={{ flex: 1, minWidth: 0 }}>
          <div className="chat-title">
            {root ? (root.name || root.path) : 'Pinkbin AI'}
          </div>
          <div className="chat-sub">
            {root
              ? `${root.path} · ${formatBytes(root.size)} · ${root.file_count.toLocaleString()} 文件`
              : '随时开始对话，扫描后可结合磁盘分析'}
          </div>
        </div>
        {chat.turns.length > 0 && (
          <button className="ghost icon" onClick={resetChat} title="清空对话" aria-label="清空对话" disabled={chat.busy}><X size={16} /></button>
        )}
      </div>

      <div className="chat-scroll" ref={scrollerRef}>
        {empty && !root && (
          <div className="chat-hero">
            <MessageSquare size={32} />
            <h3>Pinkbin AI</h3>
            <p>可以先聊聊你想清理什么，或贴张图片问我。<br />需要判断具体文件时，再扫描磁盘，把文件或文件夹拖进来。</p>
            {!isTauri && <p className="muted">浏览器预览模式：扫描数据是模拟的，但 AI 会走真实接口。</p>}
          </div>
        )}
        {empty && root && (
          <div className="chat-hero">
            <Sparkles size={28} />
            <p>可以围绕扫描结果提问，也可以把左侧文件或文件夹拖进来。</p>
          </div>
        )}
        {chat.turns.map((t) => (
          <div key={t.id} className={'chat-turn ' + t.role + (t.pending ? ' pending' : '')}>
            {t.role === 'assistant' && t.advice && <AdviceCard advice={t.advice} />}
            <div className="chat-bubble">
              {t.role === 'assistant'
                ? <ReactMarkdown remarkPlugins={[remarkGfm]}>{t.text}</ReactMarkdown>
                : t.text}
            </div>
            {t.role === 'assistant' && <ReplyTiming turn={t} />}
            {t.role === 'assistant' && t.advice?.safe_to_delete && t.advice.risk === 'low' && ['recycle', 'delete'].includes(t.advice.action) && !t.advice.needs_inspection && node && (
              <div className="chat-actions">
                <button className="primary" onClick={() => deleteNode(node, t.advice?.reasoning ?? 'AI suggested')}>
                  <Trash2 size={13} /> 直接删除 {formatBytes(node.size)}
                </button>
              </div>
            )}
          </div>
        ))}
        {chat.busy && <div className="chat-typing">AI 正在打字…</div>}
      </div>

      <div className="chat-input-wrap">
        {pendingDrops.length > 0 && (
          <div className="chat-pills">
            {pendingDrops.map((d) => (
              <span key={d.path} className="chat-pill" title={d.path}>
                {d.path.endsWith(d.name) && d.path !== d.name ? <Folder size={11} /> : <File size={11} />}
                {d.name}
                <button onClick={() => setPendingDrops((prev) => prev.filter((p) => p.path !== d.path))}><X size={11} /></button>
              </span>
            ))}
          </div>
        )}
        {pendingImages.length > 0 && (
          <div className="chat-image-pills">
            {pendingImages.map((img) => (
              <span key={img.id} className="chat-image-pill" title={img.name}>
                <img src={img.dataUrl} alt={img.name} />
                <button
                  type="button"
                  onClick={() => setPendingImages((prev) => prev.filter((p) => p.id !== img.id))}
                ><X size={11} /></button>
              </span>
            ))}
          </div>
        )}
        <input
          ref={fileInputRef}
          type="file"
          accept="image/*"
          multiple
          style={{ display: 'none' }}
          onChange={async (e) => {
            const files = Array.from(e.target.files ?? []);
            for (const f of files) await addImageFile(f);
            if (fileInputRef.current) fileInputRef.current.value = '';
          }}
        />
        <div className="chat-input">
          <button
            type="button"
            className="ghost icon chat-attach"
            onClick={() => fileInputRef.current?.click()}
            title="加图片（也可以粘贴/拖进来）"
            disabled={chat.busy}
          >
            <ImagePlus size={15} />
          </button>
          <textarea
            rows={2}
            aria-label="消息"
            placeholder={root ? '问 AI：这是什么？能删吗？把文件 / 图片拖进来…（图片粘贴也行）' : '直接输入问题，或贴张图片来问…'}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onPaste={onPaste}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                askFollowUp();
              }
            }}
          />
          <button
            className="primary"
            onClick={askFollowUp}
            disabled={
              (!input.trim() && pendingDrops.length === 0 && pendingImages.length === 0) ||
              chat.busy
            }
          >
            <Send size={14} /> 发送
          </button>
        </div>
      </div>
    </div>
  );
}

function AdviceCard({ advice }: { advice: AdvisorResponse }) {
  const Icon = advice.risk === 'low' ? ShieldCheck : advice.risk === 'medium' ? ShieldAlert : ShieldX;
  const color = advice.risk === 'low' ? '#5fcf95' : advice.risk === 'medium' ? '#ffb37a' : '#ff5d7a';
  return (
    <div className="advice-pill" style={{ borderColor: color }}>
      <Icon size={14} style={{ color }} />
      <strong>{advice.category}</strong>
      <span className="badge">{advice.action}</span>
      <span className="muted">风险 {advice.risk}</span>
      {advice.needs_inspection && <span className="badge">需要再看看</span>}
    </div>
  );
}

