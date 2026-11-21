export type ActionState = { phase: 'idle' | 'pending' | 'success' | 'error'; message: string };

/** The synchronous gate prevents double invocation even before React renders. */
export function createStatusAction(onState: (state: ActionState) => void) {
  let busy = false;
  return async (operation: () => Promise<string>, timeoutMs = 22000) => {
    if (busy) return;
    busy = true;
    onState({ phase: 'pending', message: '' });
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      const message = await Promise.race([
        operation(),
        new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error('Operation timed out. Recheck Codex status before retrying.')), timeoutMs); }),
      ]);
      onState({ phase: 'success', message });
    } catch (error) {
      onState({ phase: 'error', message: error instanceof Error ? error.message : String(error) });
    } finally {
      clearTimeout(timer);
      busy = false;
    }
  };
}
