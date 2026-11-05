import { listen } from '@tauri-apps/api/event';
import { isTauri } from './env';

export const SHARED_SCAN_INVALIDATED = 'shared-scan-invalidated';
export async function onSharedScanInvalidated(callback: () => void): Promise<() => void> {
  if (isTauri) return listen(SHARED_SCAN_INVALIDATED, callback);
  window.addEventListener(SHARED_SCAN_INVALIDATED, callback);
  return () => window.removeEventListener(SHARED_SCAN_INVALIDATED, callback);
}
