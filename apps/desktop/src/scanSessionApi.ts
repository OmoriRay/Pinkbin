import { invoke } from '@tauri-apps/api/core';
import { isTauri } from './env';
import { scan as demoScan } from './mocks';
import type { Node } from './types';
import { startDemoInventory, pollDemoInventory, demoInventoryEpoch, type DemoRequest } from './sharedScanDemo';

export interface ScanStatsEvent {
  mode: string; mft_attempted: boolean; mft_succeeded: boolean; mft_ms: number;
  walk_ms: number; build_tree_ms: number; scanner_total_ms: number; tag_ms: number;
  cmd_total_ms: number; files_seen: number; bytes_seen: number; dirs_in_acc: number;
}
export interface ScanSession {
  scan_id: string;
  path: string;
  status: 'running' | 'completed' | 'failed';
  progress: { files_seen: number; bytes_seen: number; current_path: string } | null;
  result: Node | null;
  error: string | null;
  stats: ScanStatsEvent | null;
  epoch?: number;
  invalidated?: boolean;
  result_key?: string | null;
}

// Preview only: real scan results and cleanup capabilities remain in Rust memory.
const DEMO_KEY = 'pinkbin.demo.scan-session.v1';
interface DemoSession { session: ScanSession; startedAt: number; request?: DemoRequest }
function readDemo(): DemoSession | null {
  try { return JSON.parse(sessionStorage.getItem(DEMO_KEY) || 'null'); } catch { return null; }
}
let demo = readDemo();
function saveDemo(value: DemoSession) {
  demo = value;
  try { sessionStorage.setItem(DEMO_KEY, JSON.stringify(value)); } catch { /* Preview storage may be unavailable. */ }
}
export async function startScanSession(path: string, force = false): Promise<ScanSession> {
  if (isTauri) return invoke('start_scan_session', { path, force });
  if (demo?.session.status === 'running') return (await getScanSession())!;
  const request = startDemoInventory([path], force);
  const session: ScanSession = { scan_id: crypto.randomUUID(), path, status: 'running', progress: null, result: null, error: null, stats: null, epoch: request.epoch };
  saveDemo({ session, startedAt: Date.now(), request });
  return session;
}
export async function getScanSession(): Promise<ScanSession | null> {
  if (isTauri) return invoke('get_scan_session');
  const current = demo;
  if (current && current.session.epoch !== undefined && current.session.epoch !== demoInventoryEpoch()) {
    saveDemo({ ...current, session: { ...current.session, status: 'failed', invalidated: true, result: null, error: '文件列表可能已变化，请重新扫描以更新' } });
    return demo!.session;
  }
  if (!current || current.session.status !== 'running') return current?.session ?? null;
  const elapsed = Date.now() - current.startedAt;
  const progress = current.request ? pollDemoInventory(current.request) : null;
  const result = (progress ? progress.completed : elapsed >= 2000) ? await demoScan(current.session.path, true) : null;
  if (demo !== current) return demo?.session ?? null;
  const session: ScanSession = {
    ...current.session,
    status: result ? 'completed' : 'running', result,
    result_key: result && progress ? `${progress.key}:${current.session.path}` : null,
    stats: result ? { mode: current.request?.reused ? 'shared_reuse' : 'shared_walk', mft_attempted: false, mft_succeeded: false, mft_ms: 0, walk_ms: elapsed, build_tree_ms: 0, scanner_total_ms: elapsed, tag_ms: 0, cmd_total_ms: elapsed, files_seen: result.file_count, bytes_seen: result.size, dirs_in_acc: 0 } : null,
    progress: { files_seen: Math.floor(elapsed * 120), bytes_seen: Math.floor(elapsed * 20_000_000), current_path: current.session.path },
  };
  saveDemo({ ...current, session });
  return session;
}
