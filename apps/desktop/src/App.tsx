import { useEffect, useState } from 'react';
import { Brush, Folder, ScanLine, Settings as SettingsIcon } from 'lucide-react';
import { open } from '@tauri-apps/plugin-dialog';
import { isTauri } from './env';
import { useStore } from './store';
import { TreeView } from './components/TreeView';
import { ChatPanel } from './components/ChatPanel';
import { Settings } from './components/Settings';
import { Splitter } from './components/Splitter';
import { Logo } from './components/Logo';
import { CacheCleaner } from './components/CacheCleaner';
import { ErrorBoundary } from './components/ErrorBoundary';
import { formatBytes } from './format';
import { loadSettings, isConfigured } from './advisorClient';
import { loadAppInfo, type AppInfoState } from './appInfo';
import { useScanSession, type ScanDiag } from './useScanSession';
import { getDiskCleanupScan } from './diskCleanerApi';
import { getCacheCleanupScan } from './cacheCleanerApi';

const DEFAULT_LEFT = 620;
const MIN_LEFT = 320;
const MIN_CENTER = 360;

export default function App() {
  const root = useStore((s) => s.root);
  const selectedPath = useStore((s) => s.selectedPath);
  const select = useStore((s) => s.selectPath);

  const { pickedPath, setPickedPath, scanning, restoring, scanProgress, err, diag, scan } = useScanSession();
  const [showSettings, setShowSettings] = useState(false);
  const [showCacheCleaner, setShowCacheCleaner] = useState(false);
  const [advisorTag, setAdvisorTag] = useState<{ provider: string } | null>(null);
  const [appInfo, setAppInfo] = useState<AppInfoState>({ status: 'loading' });
  const refreshAdvisorTag = () => {
    const s = loadSettings();
    setAdvisorTag(isConfigured(s) ? { provider: s.provider } : null);
  };
  useEffect(() => { refreshAdvisorTag(); }, []);
  useEffect(() => {
    let active = true;
    loadAppInfo().then(
      (info) => { if (active) setAppInfo({ status: 'ready', info }); },
      () => { if (active) setAppInfo({ status: 'error' }); },
    );
    return () => { active = false; };
  }, []);
  const [leftWidth, setLeftWidth] = useState<number>(() => {
    const v = Number(localStorage.getItem('pinkbin.leftWidth'));
    return Number.isFinite(v) && v > MIN_LEFT ? v : DEFAULT_LEFT;
  });
  useEffect(() => { localStorage.setItem('pinkbin.leftWidth', String(leftWidth)); }, [leftWidth]);

  const dragLeft = (dx: number) => {
    setLeftWidth((w) => {
      const winW = window.innerWidth;
      const maxLeft = Math.max(MIN_LEFT, winW - 4 - MIN_CENTER);
      return Math.max(MIN_LEFT, Math.min(maxLeft, w + dx));
    });
  };
  useEffect(() => {
    let active = true;
    // A refreshed window reconnects to the active disk check without starting another.
    void Promise.allSettled([getDiskCleanupScan(), getCacheCleanupScan()]).then(sessions => {
      if (active && sessions.some(item => item.status === 'fulfilled' && item.value?.status === 'running')) setShowCacheCleaner(true);
    }).catch(() => { /* The panel can reconnect when opened manually. */ });
    return () => { active = false; };
  }, []);

  const pickDirectory = async () => {
    if (!isTauri) {
      const p = window.prompt('浏览器预览模式：输入一个路径（任意值都可以）', 'C:\\');
      if (p) setPickedPath(p);
      return;
    }
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked === 'string') setPickedPath(picked);
  };

  return (
    <div className="app">
      <header>
        <span className="brand"><Logo size={22} /> Pinkbin</span>
        <button className="ghost directory-picker" onClick={pickDirectory} disabled={scanning || restoring} title={pickedPath || '选择磁盘或文件夹'}>
          <Folder size={14} /><span>{pickedPath || '选择磁盘或文件夹'}</span>
        </button>
        <button className="primary" onClick={scan} disabled={!pickedPath || scanning || restoring}>
          <ScanLine size={14} /> {restoring ? '正在连接…' : scanning ? '扫描中…' : '扫描'}
        </button>
        <div className="grow" />
        <button className="ghost cache-cleaner-trigger" onClick={() => setShowCacheCleaner(true)}>
          <Brush size={15} />磁盘清理
        </button>
        <span className="muted small">
          {root ? `${formatBytes(root.size)} · ${root.file_count.toLocaleString()} 文件` : '未扫描'}
        </span>
        <button
          className={'ghost icon settings-btn' + (advisorTag ? ' bound' : '')}
          onClick={() => setShowSettings(true)}
          title={advisorTag ? 'AI 设置 · 已保存配置' : 'AI 设置 · 尚未配置'}
        >
          <SettingsIcon size={16} />
        </button>
        <button className={`provider-pill${advisorTag ? '' : ' unconfigured'}`} onClick={() => setShowSettings(true)} title={advisorTag ? '已保存 AI 配置；点击查看设置或测试连接' : '点击配置 AI 顾问'}>
          {advisorTag ? 'AI 已配置' : 'AI 未配置'}
        </button>
      </header>

      {scanning && (
        <div className="scan-bar">
          <div
            className="scan-bar-fill indeterminate"
          />
          <div className="scan-bar-label">
            {scanProgress
              ? `${scanProgress.files.toLocaleString()} 个文件 · ${formatBytes(scanProgress.bytes)}`
              : '准备扫描…'}
          </div>
        </div>
      )}
      {diag && !scanning && <DiagnosticsBar diag={diag} />}
      {err && <div className="banner error">{err}</div>}

      <main style={{ gridTemplateColumns: `clamp(${MIN_LEFT}px, ${leftWidth}px, calc(100% - ${MIN_CENTER + 4}px)) 4px minmax(0, 1fr)` }}>
        <aside className="left">
          {root ? <TreeView root={root} selectedPath={selectedPath} onSelect={select} /> : <EmptyLeft />}
        </aside>

        <Splitter onDrag={dragLeft} onDoubleClick={() => setLeftWidth(DEFAULT_LEFT)} />

        <section className="center">
          <ChatPanel />
        </section>

      </main>

      <footer>
        <span className="app-version" title={appInfo.status === 'ready' ? `构建日期 ${appInfo.info.build_date}${isTauri ? '' : ' · 浏览器演示'}` : undefined}>
          {appInfo.status === 'ready' ? `Pinkbin ${appInfo.info.edition} v${appInfo.info.version}` : `Pinkbin · ${appInfo.status === 'loading' ? '正在读取版本…' : '版本暂不可读取'}`}
        </span>
        <span className="footer-path" title={root?.path}>{root?.path ?? '还没扫描'}</span>
      </footer>

      {showSettings && <Settings appInfo={appInfo} onClose={() => { setShowSettings(false); refreshAdvisorTag(); }} />}
      {showCacheCleaner && <ErrorBoundary fallbackLabel="磁盘清理面板发生错误"><CacheCleaner onClose={() => setShowCacheCleaner(false)} /></ErrorBoundary>}
    </div>
  );
}

function EmptyLeft() {
  return (
    <div className="empty">
      <div className="empty-title">还没扫描</div>
      <div className="empty-sub">在顶栏选一个文件夹，然后点「扫描」。<br />扫完之后，左侧会列出每个文件夹和文件。</div>
    </div>
  );
}

function fmtMs(ms: number | null | undefined): string {
  if (ms == null) return '—';
  if (ms < 1000) return `${ms.toFixed(0)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

function DiagnosticsBar({ diag }: { diag: ScanDiag }) {
  const b = diag.backend;
  const parts: string[] = [];
  if (b) {
    parts.push(`mode=${b.mode}`);
    if (b.mft_attempted) parts.push(`mft=${b.mft_succeeded ? 'ok' : 'fail'}/${fmtMs(b.mft_ms)}`);
    if (b.mode === 'walkdir') {
      parts.push(`walk=${fmtMs(b.walk_ms)}`);
      parts.push(`build_tree=${fmtMs(b.build_tree_ms)}`);
      parts.push(`dirs=${b.dirs_in_acc.toLocaleString()}`);
    }
    parts.push(`scanner=${fmtMs(b.scanner_total_ms)}`);
    parts.push(`tag=${fmtMs(b.tag_ms)}`);
  }
  parts.push(`ipc=${fmtMs(diag.ipcMs)}`);
  parts.push(`setRoot=${fmtMs(diag.setRootMs)}`);
  parts.push(`total=${fmtMs(diag.totalMs)}`);
  return (
    <div className="diag-bar" title="扫描各阶段耗时">
      <span className="diag-label">诊断</span>
      <span className="diag-stats">{parts.join(' · ')}</span>
    </div>
  );
}
