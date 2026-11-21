import { type ClassValue, clsx } from 'clsx';
import { twMerge } from 'tailwind-merge';

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

export function formatBytes(bytes: number, digits = 1) {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1000)), units.length - 1);
  return `${(bytes / 1000 ** index).toFixed(index === 0 ? 0 : digits)} ${units[index]}`;
}

export function formatRelativeTime(timestamp: number | null | undefined, locale: 'en' | 'zh' = 'zh') {
  if (!timestamp) return locale === 'en' ? 'Unknown' : '未知';
  const elapsed = Date.now() - timestamp;
  const minutes = Math.max(0, Math.round(elapsed / 60_000));
  if (minutes < 1) return locale === 'en' ? 'Just now' : '刚刚';
  if (minutes < 60) return locale === 'en' ? `${minutes}m ago` : `${minutes} 分钟前`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return locale === 'en' ? `${hours}h ago` : `${hours} 小时前`;
  const days = Math.round(hours / 24);
  return locale === 'en' ? `${days}d ago` : `${days} 天前`;
}
