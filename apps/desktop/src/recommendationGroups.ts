import type { RecommendedGroup } from './diskCleanerApi';

const categories = [
  { id: 'application_cache', title: '应用缓存' },
  { id: 'build_cache', title: '编译与下载缓存' },
  { id: 'diagnostics', title: '旧日志与故障记录' },
  { id: 'temporary', title: '临时与安装残留' },
  { id: 'other', title: '其他已核验项目' },
] as const;

export function groupRecommendations(groups: readonly RecommendedGroup[]) {
  const buckets = categories.map(category => ({ ...category, groups: [] as RecommendedGroup[], bytes: 0, fileCount: 0 }));
  for (const group of groups) {
    const bucket = buckets.find(item => item.id === group.category) ?? buckets[buckets.length - 1];
    bucket.groups.push(group);
    bucket.bytes += group.bytes;
    bucket.fileCount += group.file_count;
  }
  return buckets.filter(bucket => bucket.groups.length > 0);
}
