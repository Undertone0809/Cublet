import type { UpdateCommand, UpdateState } from './lib/updates';
export {};

declare global {
  interface Window {
    electronAPI?: {
      updates?: {
        command(command: UpdateCommand): Promise<UpdateState>;
        subscribe(listener: (state: UpdateState) => void): () => void;
      };
      invoke<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T>;
    };
  }
}
