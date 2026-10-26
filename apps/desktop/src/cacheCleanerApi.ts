import { invoke } from '@tauri-apps/api/core';
import { isTauri } from './env';

export interface CacheRule {
  id: string;
  title: string;
  description: string;
  consequence: string;
  retention_days: number;
  file_count: number;
  bytes: number;
  samples: string[];
  status: 'ready' | 'empty' | 'blocked';
  message: string | null;
}

export interface CachePlan {
  plan_id: string;
  created_at: string;
  rules: CacheRule[];
  warnings: string[];
}
export interface CacheScanSession {
  scan_id: string;
  status: 'running' | 'completed' | 'failed';
  preview: CachePlan | null;
  error: string | null;
}

export interface CacheResult {
  recycled_files: number;
  recycled_bytes: number;
  skipped_files: number;
  failed_files: number;
  messages: string[];
  log_path: string;
  history_warning?: string | null;
}

export interface CleanupHistoryRecord {
  id: string;
  plan_id: string;
  started_at: string;
  finished_at: string | null;
  selected_rules: { id: string; title: string }[];
  status: 'completed' | 'partial' | 'failed' | 'incomplete';
  recycled_files: number;
  recycled_bytes: number;
  skipped_files: number;
  failed_files: number;
  message: string | null;
}

export interface CleanupHistory {
  records: CleanupHistoryRecord[];
  warnings: string[];
}

// Explicit fixture data only; desktop preview always uses backend rules.
// New backend rule ids are rendered dynamically by CacheCleaner.
const demoElectronRules: CacheRule[] = [
  ['cherry-studio-cache', 'Cherry Studio 缓存'],
  ['chatbox-cache', 'Chatbox 缓存'],
  ['lx-music-cache', '洛雪音乐缓存'],
  ['docker-desktop-cache', 'Docker Desktop 缓存'],
  ['opencode-cache', 'OpenCode 缓存'],
].map(([id, title], index) => ({
  id, title,
  description: id === 'docker-desktop-cache'
    ? '应用关闭后清理默认界面缓存，不涉及镜像、容器、卷或虚拟磁盘'
    : id === 'lx-music-cache'
    ? '应用关闭后清理默认网页、代码和 GPU 缓存，不涉及歌单或下载目录'
    : '应用关闭后清理默认目录中的网页、代码和 GPU 缓存，不涉及聊天或账号数据',
  consequence: id === 'lx-music-cache'
    ? '移入回收站；页面与音频缓存可能需要重新下载，首次打开可能稍慢'
    : '移入回收站；应用可能重新生成缓存，首次打开可能稍慢',
  retention_days: 0,
  file_count: 12 + index,
  bytes: (20 + index * 5) * 1024 * 1024,
  samples: [], status: 'ready', message: null,
}));

const demoRules: CacheRule[] = [
  { id: 'temp', title: '用户临时文件', description: '应用留下的临时文件与安装残留', consequence: '正在使用的文件会跳过；应用需要时会重新生成。', retention_days: 7, file_count: 146, bytes: 187695104, samples: ['C:\\Users\\DEMO\\AppData\\Local\\Temp\\example.tmp'], status: 'ready', message: null },
  { id: 'chrome', title: 'Chrome 缓存', description: '网页图片、脚本等可重新下载的缓存', consequence: '首次打开网页可能稍慢；不清理密码、书签或登录信息。', retention_days: 0, file_count: 382, bytes: 293601280, samples: ['C:\\Users\\DEMO\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Cache\\Cache_Data\\example'], status: 'ready', message: null },
  { id: 'edge', title: 'Edge 缓存', description: '网页图片、脚本等可重新下载的缓存', consequence: '首次打开网页可能稍慢；不清理密码、书签或登录信息。', retention_days: 0, file_count: 95, bytes: 67108864, samples: ['C:\\Users\\DEMO\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Cache\\Cache_Data\\example'], status: 'blocked', message: '请先关闭 Edge 后重新扫描' },
  { id: 'brave', title: 'Brave 缓存', description: '网页图片、脚本等可重新下载的缓存', consequence: '网页首次打开可能稍慢。', retention_days: 0, file_count: 84, bytes: 52428800, samples: [], status: 'ready', message: null },
  { id: 'firefox', title: 'Firefox 缓存', description: '网页图片、脚本等可重新下载的缓存', consequence: '网页首次打开可能稍慢。', retention_days: 0, file_count: 0, bytes: 0, samples: [], status: 'empty', message: null },
  { id: 'pip-cache', title: 'Python 下载缓存', description: '下载过的软件包缓存', consequence: '以后安装 Python 软件包可能需重新下载或构建，离线安装也可能受影响。', retention_days: 30, file_count: 47, bytes: 157286400, samples: [], status: 'ready', message: null },
  { id: 'npm-cache', title: 'npm 下载缓存', description: '下载过的软件包缓存', consequence: '以后安装软件包可能需要重新下载，离线安装可能受影响。', retention_days: 30, file_count: 76, bytes: 104857600, samples: [], status: 'ready', message: null },
  ...demoElectronRules,
  { id: 'crash-dumps', title: '应用崩溃转储', description: '用于排查应用异常的诊断文件', consequence: '清理后将无法用这些转储排查旧故障，建议按需选择。', retention_days: 30, file_count: 3, bytes: 83886080, samples: ['C:\\Users\\DEMO\\AppData\\Local\\CrashDumps\\example.dmp'], status: 'ready', message: null },
];

let demoPlan: CachePlan | null = null;
let demoRecycled = new Set<string>();
const demoHistory: CleanupHistoryRecord[] = [];
export function addDemoCleanupHistory(record: CleanupHistoryRecord) { demoHistory.unshift(record); }
const delay = () => new Promise<void>((resolve) => window.setTimeout(resolve, 650));
const demoSessionKey = 'pinkbin-demo-cache-session';
type DemoCacheSession = { session: CacheScanSession; startedAt: number };
function readDemoSession(): DemoCacheSession | null {
  try { return JSON.parse(sessionStorage.getItem(demoSessionKey) || 'null'); } catch { return null; }
}
let demoSession: DemoCacheSession | null = isTauri ? null : readDemoSession();
function saveDemoSession(value: DemoCacheSession | null) {
  demoSession = value;
  try { if (value) sessionStorage.setItem(demoSessionKey, JSON.stringify(value)); else sessionStorage.removeItem(demoSessionKey); } catch { /* 演示存储不可用时保留当前页面会话。 */ }
}
export async function startCacheCleanupScan(): Promise<CacheScanSession> {
  if (isTauri) return invoke('start_cache_cleanup_scan');
  if (demoSession?.session.status === 'running') return (await getCacheCleanupScan())!;
  const session: CacheScanSession = { scan_id: crypto.randomUUID(), status: 'running', preview: null, error: null };
  saveDemoSession({ session, startedAt: Date.now() });
  return session;
}
export async function getCacheCleanupScan(): Promise<CacheScanSession | null> {
  if (isTauri) return invoke('get_cache_cleanup_scan');
  if (!demoSession) return null;
  if (demoSession.session.status === 'running' && Date.now() - demoSession.startedAt >= 1800) {
    saveDemoSession({ ...demoSession, session: { ...demoSession.session, status: 'completed', preview: createDemoPlan() } });
  }
  const preview = demoSession!.session.preview;
  if (preview && Date.now() - new Date(preview.created_at).getTime() > 15 * 60 * 1000) {
    saveDemoSession({ ...demoSession!, session: { ...demoSession!.session, status: 'failed', preview: null, error: '预览已过期，请重新扫描。' } });
    demoPlan = null;
  } else if (preview) demoPlan = preview;
  return demoSession!.session;
}

export async function previewCacheCleanup(): Promise<CachePlan> {
  if (isTauri) return invoke<CachePlan>('preview_cache_cleanup');
  await delay();
  return createDemoPlan();
}
function createDemoPlan(): CachePlan {
  demoPlan = {
    plan_id: `demo-${Date.now()}`,
    created_at: new Date().toISOString(),
    rules: demoRules.map((rule) => demoRecycled.has(rule.id)
      ? { ...rule, status: 'empty', file_count: 0, bytes: 0, samples: [] }
      : { ...rule }),
    warnings: [],
  };
  return demoPlan;
}

export async function executeCacheCleanup(planId: string, ruleIds: string[]): Promise<CacheResult> {
  if (isTauri) return invoke<CacheResult>('execute_cache_cleanup', { planId, ruleIds });
  if (!demoPlan || demoPlan.plan_id !== planId) throw new Error('演示预览已过期，请重新扫描。');
  const selected = demoPlan.rules.filter((rule) => ruleIds.includes(rule.id) && rule.status === 'ready');
  demoPlan = null; saveDemoSession(null);
  await delay();
  const result = {
    recycled_files: selected.reduce((sum, rule) => sum + rule.file_count, 0),
    recycled_bytes: selected.reduce((sum, rule) => sum + rule.bytes, 0),
    skipped_files: 0, failed_files: 0,
    messages: ['DEMO：仅模拟移入回收站，没有读取或更改本机文件。'],
    log_path: '',
  };
  selected.forEach((rule) => demoRecycled.add(rule.id));
  const now = new Date().toISOString();
  demoHistory.unshift({ id: `history-${Date.now()}`, plan_id: planId, started_at: now, finished_at: now, selected_rules: selected.map(({ id, title }) => ({ id, title })), status: 'completed', recycled_files: result.recycled_files, recycled_bytes: result.recycled_bytes, skipped_files: 0, failed_files: 0, message: '浏览器演示记录，仅在当前页面中保留。' });
  demoPlan = null;
  return result;
}

export async function listCleanupHistory(): Promise<CleanupHistory> {
  if (isTauri) return invoke<CleanupHistory>('list_cleanup_history');
  await delay();
  return { records: demoHistory.slice(0, 100), warnings: [] };
}
