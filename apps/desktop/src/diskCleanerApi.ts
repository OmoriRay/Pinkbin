import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { isTauri } from './env';
import { addDemoCleanupHistory, type CacheResult } from './cacheCleanerApi';
export interface CleanupDrive { id: string; label: string; total_bytes: number; available_bytes: number }
export interface DiskEntry { id: string; category: string; title: string; risk: string; size: number; path: string; modified: string | number | null; default_selected: boolean }
export interface DiskGroup { id: string; title: string; description?: string; risk: string; file_count: number; bytes: number; entries: DiskEntry[] }
export interface DiskProgress { scan_id: string; drive_id: string; scanned_files: number; scanned_bytes: number; scanned_directories: number; denied_directories: number; protected_directories: number; linked_entries: number }
export interface DiskPlan { plan_id: string; scan_id: string; created_at: string; groups: DiskGroup[]; stats: Omit<DiskProgress, 'scan_id' | 'drive_id'> & { limited: boolean; cancelled: boolean }; warnings: string[] }
export interface DiskScanSession { scan_id: string; drive_ids: string[]; status: 'running' | 'completed' | 'failed' | 'cancelled'; progress: DiskProgress | null; preview: DiskPlan | null; error: string | null }
const demoSessionKey = 'pinkbin-demo-disk-session';
type DemoSession = { session: DiskScanSession; startedAt: number };
function readDemoSession(): DemoSession | null {
  try { return JSON.parse(sessionStorage.getItem(demoSessionKey) || 'null'); } catch { return null; }
}
function saveDemoSession(value: DemoSession | null) {
  try { if (value) sessionStorage.setItem(demoSessionKey, JSON.stringify(value)); else sessionStorage.removeItem(demoSessionKey); } catch { /* 演示存储不可用时仍可使用当前页面。 */ }
  demoSession = value;
}
let demoSession: DemoSession | null = isTauri ? null : readDemoSession();
const GiB = 1024 ** 3;
const wait = (ms = 300) => new Promise<void>(resolve => setTimeout(resolve, ms));
const cancelled = new Set<string>();
const listeners = new Set<(progress: DiskProgress) => void>();
let demoPlan: DiskPlan | null = null;
const recycled = new Set<string>();
export async function listCleanupDrives(): Promise<CleanupDrive[]> {
  if (isTauri) return invoke('list_cleanup_drives');
  return [{ id: 'C:\\', label: '本地磁盘 (C:)', total_bytes: 512 * GiB, available_bytes: 83 * GiB }, { id: 'D:\\', label: '资料 (D:)', total_bytes: 1024 * GiB, available_bytes: 326 * GiB }];
}
export async function onDiskProgress(callback: (progress: DiskProgress) => void): Promise<() => void> {
  if (isTauri) return listen<DiskProgress>('disk-cleanup-progress', event => callback(event.payload));
  listeners.add(callback); return () => { listeners.delete(callback); };
}
export async function cancelDiskCleanupScan(scanId: string): Promise<boolean> {
  if (isTauri) return invoke('cancel_disk_cleanup_scan', { scanId });
  if (demoSession?.session.scan_id === scanId && demoSession.session.status === 'running') saveDemoSession({ ...demoSession, session: { ...demoSession.session, status: 'cancelled', preview: null } });
  cancelled.add(scanId); return true;
}
export async function startDiskCleanupScan(driveIds: string[], scanId: string): Promise<DiskScanSession> {
  if (isTauri) return invoke('start_disk_cleanup_scan', { driveIds, scanId });
  if (demoSession?.session.status === 'running') return (await getDiskCleanupScan())!;
  const session: DiskScanSession = { scan_id: scanId, drive_ids: driveIds, status: 'running', progress: null, preview: null, error: null };
  saveDemoSession({ session, startedAt: Date.now() });
  return session;
}
export async function getDiskCleanupScan(): Promise<DiskScanSession | null> {
  if (isTauri) return invoke('get_disk_cleanup_scan');
  if (!demoSession) return null;
  const current = demoSession;
  if (current.session.status === 'completed' && current.session.preview && Date.now() - new Date(current.session.preview.created_at).getTime() > 15 * 60 * 1000) {
    demoPlan = null;
    saveDemoSession({ ...current, session: { ...current.session, status: 'failed', preview: null, error: '预览已过期，请重新检查。' } });
    return demoSession!.session;
  }
  if (current.session.status === 'running') {
    const step = Math.min(12, Math.floor((Date.now() - current.startedAt) / 400));
    const progress: DiskProgress = { scan_id: current.session.scan_id, drive_id: current.session.drive_ids[Math.min(current.session.drive_ids.length - 1, Math.floor(step / 6))], scanned_files: step * 2351, scanned_bytes: step * 3.7 * GiB, scanned_directories: step * 173, denied_directories: 2, protected_directories: 8, linked_entries: 0 };
    const preview = step === 12 ? await createDemoPlan(current.session.drive_ids, current.session.scan_id, true) : null;
    // 异步生成结果期间可能收到显式取消；不覆盖更新后的会话。
    if (demoSession !== current) return demoSession?.session ?? null;
    saveDemoSession({ ...current, session: { ...current.session, progress, preview, status: preview ? 'completed' : 'running' } });
    listeners.forEach(fn => fn(progress));
  }
  if (demoSession?.session.preview) demoPlan = demoSession.session.preview;
  return demoSession?.session ?? null;
}
export async function previewDiskCleanup(driveIds: string[], scanId: string): Promise<DiskPlan> {
  if (isTauri) return invoke('preview_disk_cleanup', { driveIds, scanId });
  return createDemoPlan(driveIds, scanId);
}
async function createDemoPlan(driveIds: string[], scanId: string, immediate = false): Promise<DiskPlan> {
  let progress: DiskProgress = { scan_id: scanId, drive_id: driveIds[0], scanned_files: 0, scanned_bytes: 0, scanned_directories: 0, denied_directories: 0, protected_directories: 0, linked_entries: 0 };
  for (let step = 1; step <= 12; step++) {
    if (!immediate) await wait(180);
    if (cancelled.has(scanId)) break;
    progress = { ...progress, drive_id: driveIds[Math.min(driveIds.length - 1, Math.floor((step - 1) / 6))], scanned_files: step * 2351, scanned_bytes: step * 3.7 * GiB, scanned_directories: step * 173, denied_directories: 2, protected_directories: 8 };
    listeners.forEach(fn => fn(progress));
  }
  const wasCancelled = cancelled.delete(scanId);
  const definitions = [['temp', '旧临时文件', '临时文件也可能仍有用途，请先确认文件位置。', 'tmp', 72], ['crash', '旧故障记录', '排查软件故障时可能需要这些诊断资料。', 'dmp', 3], ['logs', '旧日志文件', '清理后无法再查看这些旧运行记录。', 'log', 8], ['installers', '较旧安装介质', '可能是需要保留的安装包，确认已安装且不再需要后再选择。', 'msi', 6], ['large', '大文件', '大文件不等于垃圾，可能是照片、视频或工作资料，请逐个判断。', 'mp4', 4]] as const;
  const groups: DiskGroup[] = wasCancelled ? [] : definitions.map(([id, title, description, extension, count]) => {
    const entries = Array.from({length: count}, (_, index) => {
      const drive = driveIds[index % driveIds.length];
      return { id: `${drive}${id}-${index}`, category: id, title: id === 'large' ? `旅行视频-${index + 1}.${extension}` : `${id === 'installers' ? '软件安装包' : 'example'}-${String(index + 1).padStart(3,'0')}.${extension}`, risk: 'review', size: id === 'large' ? (index + 1) * 1.3 * GiB : (index + 1) * 1024 * 1024, path: `${drive}${id === 'large' ? 'Videos\\旅行记录' : id === 'installers' ? 'Downloads' : '旧软件\\残留文件'}\\${id}-${index + 1}.${extension}`, modified: '2026-06-12T10:30:00Z', default_selected: false };
    }).filter(entry => !recycled.has(entry.id));
    return {id, title, description, risk: 'review', entries, file_count: entries.length, bytes: entries.reduce((sum, entry) => sum + entry.size, 0)};
  });
  demoPlan = { plan_id: wasCancelled ? '' : `disk-demo-${Date.now()}`, scan_id: scanId, created_at: new Date().toISOString(), groups, stats: { ...progress, cancelled: wasCancelled, limited: false }, warnings: [] };
  return demoPlan;
}
export async function executeDiskCleanup(planId: string, fileIds: string[]): Promise<CacheResult> {
  if (isTauri) return invoke('execute_disk_cleanup', { planId, fileIds });
  const current = demoPlan; demoPlan = null;
  saveDemoSession(null);
  if (!current || current.plan_id !== planId) throw new Error('预览已过期，请重新检查。');
  await wait(650);
  const entries = current.groups.flatMap(group => group.entries).filter(entry => fileIds.includes(entry.id));
  entries.forEach(entry => recycled.add(entry.id));
  const result: CacheResult = { deleted_files: entries.length, deleted_bytes: entries.reduce((sum, entry) => sum + entry.size, 0), skipped_files: 0, failed_files: 0, messages: ['DEMO：仅模拟直接删除，没有读取或更改本机文件。'], log_path: '' };
  const now = new Date().toISOString();
  addDemoCleanupHistory({ permanent_delete: true, id: `disk-history-${Date.now()}`, plan_id: planId, started_at: now, finished_at: now, selected_rules: current.groups.filter(group => group.entries.some(entry => fileIds.includes(entry.id))).map(({id,title}) => ({id,title})), status: 'completed', ...result, message: '全盘检查 · 浏览器演示记录，仅在当前页面中保留。' });
  return result;
}
