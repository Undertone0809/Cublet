import { useId, useRef, type KeyboardEvent, type ReactNode } from 'react';
import { LayoutGroup, motion, useReducedMotion } from 'framer-motion';
import { cn } from '@/lib/utils';
import '@/mova-controls.css';

export type SegmentedControlItem<T extends string> = {
  value: T;
  label: ReactNode;
  icon?: ReactNode;
  disabled?: boolean;
};

type AnimatedSegmentedControlProps<T extends string> = {
  value: T;
  onValueChange: (value: T) => void;
  items: readonly SegmentedControlItem<T>[];
  ariaLabel: string;
  className?: string;
};

// Adapted from Mova's pill-tabs (AnimatedToggle), retaining its shared-layout
// indicator and spring. See ../mova/README.md for source and attribution.
export function AnimatedSegmentedControl<T extends string>({
  value, onValueChange, items, ariaLabel, className,
}: AnimatedSegmentedControlProps<T>) {
  const id = useId();
  const buttons = useRef(new Map<T, HTMLButtonElement>());
  const reducedMotion = useReducedMotion();
  const enabledItems = items.filter((item) => !item.disabled);
  const tabStop = enabledItems.find((item) => item.value === value)?.value ?? enabledItems[0]?.value;

  function handleKeyDown(event: KeyboardEvent<HTMLButtonElement>, currentValue: T) {
    const currentIndex = enabledItems.findIndex((item) => item.value === currentValue);
    if (currentIndex < 0 || !enabledItems.length) return;
    const rtl = getComputedStyle(event.currentTarget).direction === 'rtl';
    let nextIndex: number;
    switch (event.key) {
      case 'ArrowRight': nextIndex = currentIndex + (rtl ? -1 : 1); break;
      case 'ArrowLeft': nextIndex = currentIndex + (rtl ? 1 : -1); break;
      case 'Home': nextIndex = 0; break;
      case 'End': nextIndex = enabledItems.length - 1; break;
      default: return;
    }
    event.preventDefault();
    const next = enabledItems[(nextIndex + enabledItems.length) % enabledItems.length];
    buttons.current.get(next.value)?.focus();
    if (next.value !== value) onValueChange(next.value);
  }

  return (
    <LayoutGroup id={id}>
      <div role="group" aria-label={ariaLabel} className={cn('mova-segmented-control', className)}>
        {items.map((item) => (
          <motion.button
            key={item.value}
            ref={(element) => { if (element) buttons.current.set(item.value, element); else buttons.current.delete(item.value); }}
            type="button"
            aria-pressed={value === item.value}
            disabled={item.disabled}
            tabIndex={item.value === tabStop ? 0 : -1}
            className="mova-segmented-item"
            onClick={() => { if (item.value !== value) onValueChange(item.value); }}
            onKeyDown={(event) => handleKeyDown(event, item.value)}
            whileTap={reducedMotion ? undefined : { scale: 0.97 }}
            transition={{ type: 'spring', stiffness: 500, damping: 30 }}
          >
            {value === item.value && (
              <motion.span
                aria-hidden="true"
                className="mova-segmented-indicator"
                layoutId="pill-tabs-indicator"
                initial={false}
                transition={reducedMotion ? { duration: 0 } : { type: 'spring', stiffness: 500, damping: 30 }}
              />
            )}
            <span className="mova-segmented-label">
              {item.icon && <span className="mova-segmented-icon" aria-hidden="true">{item.icon}</span>}
              {item.label}
            </span>
          </motion.button>
        ))}
      </div>
    </LayoutGroup>
  );
}
