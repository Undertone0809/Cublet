export type UpdateState = {
  status: 'idle' | 'unavailable' | 'checking' | 'current' | 'available' | 'downloading' | 'ready' | 'installing' | 'error';
  currentVersion: string;
  latestVersion: string | null;
  percent: number;
  error: 'offline' | 'signature' | 'failed' | null;
  retry: 'check' | 'download' | 'install';
  checkedAt: number | null;
};
export type UpdateCommand = 'state' | 'check' | 'download' | 'install';
