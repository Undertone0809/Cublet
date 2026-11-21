import { Children, Fragment, isValidElement, type ComponentProps, type ReactNode } from 'react';
import { Select as SelectPrimitive } from '@base-ui/react/select';
import { CheckIcon, ChevronDownIcon } from 'lucide-react';
import { cn } from '@/lib/utils';
import '@/mova-controls.css';

type NativeSelectProps = Omit<ComponentProps<'button'>, 'children' | 'value' | 'defaultValue' | 'onChange'> & {
  children: ReactNode;
  value?: string;
  defaultValue?: string;
  onValueChange?: (value: string) => void;
  size?: 'sm' | 'default';
  name?: string;
  required?: boolean;
};
type Option = { value: string; label: ReactNode; disabled: boolean; group?: string };

// Preserve existing option/Fragment call sites; Base UI owns selection, typeahead,
// arrow keys, overlay positioning, and focus return. See ../mova/README.md.
function readOptions(children: ReactNode, group?: string, groupDisabled = false): Option[] {
  const options: Option[] = [];
  Children.forEach(children, (child) => {
    if (!isValidElement<ComponentProps<'option'> & { label?: string }>(child)) return;
    if (child.type === Fragment) {
      options.push(...readOptions(child.props.children, group, groupDisabled));
    } else if (child.type === 'optgroup' || child.type === NativeSelectOptGroup) {
      options.push(...readOptions(child.props.children, child.props.label, groupDisabled || !!child.props.disabled));
    } else if (child.type === 'option' || child.type === NativeSelectOption) {
      options.push({ value: String(child.props.value ?? ''), label: child.props.children, disabled: groupDisabled || !!child.props.disabled, group });
    }
  });
  return options;
}

function NativeSelect({ className, size = 'default', children, value, defaultValue, onValueChange, disabled, name, required, ...props }: NativeSelectProps) {
  const options = readOptions(children);
  return (
    <SelectPrimitive.Root
      value={value}
      defaultValue={defaultValue}
      onValueChange={(next) => { if (next !== null) onValueChange?.(next); }}
      items={options.map(({ value: itemValue, label }) => ({ value: itemValue, label }))}
      disabled={disabled}
      name={name}
      required={required}
    >
      <div className={cn('mova-select-wrapper', className)} data-slot="native-select-wrapper" data-size={size}>
        <SelectPrimitive.Trigger {...props} data-slot="native-select" data-size={size} className="mova-select-trigger">
          <SelectPrimitive.Value className="mova-select-value" />
          <SelectPrimitive.Icon className="mova-select-icon"><ChevronDownIcon size={14} aria-hidden="true" /></SelectPrimitive.Icon>
        </SelectPrimitive.Trigger>
      </div>
      <SelectPrimitive.Portal>
        <SelectPrimitive.Positioner sideOffset={5} align="start" alignItemWithTrigger={false} className="mova-select-positioner">
          <SelectPrimitive.Popup className="mova-select-popup">
            <SelectPrimitive.List className="mova-select-list">
              {options.map((option, index) => (
                <Fragment key={option.value}>
                  {option.group && options[index - 1]?.group !== option.group && <div className="mova-select-group" aria-hidden="true">{option.group}</div>}
                  <SelectPrimitive.Item value={option.value} disabled={option.disabled} className="mova-select-item">
                    <SelectPrimitive.ItemText className="mova-select-label">{option.label}</SelectPrimitive.ItemText>
                    <SelectPrimitive.ItemIndicator className="mova-select-check"><CheckIcon size={14} aria-hidden="true" /></SelectPrimitive.ItemIndicator>
                  </SelectPrimitive.Item>
                </Fragment>
              ))}
            </SelectPrimitive.List>
          </SelectPrimitive.Popup>
        </SelectPrimitive.Positioner>
      </SelectPrimitive.Portal>
    </SelectPrimitive.Root>
  );
}

function NativeSelectOption(props: ComponentProps<'option'>) { return <option {...props} />; }
function NativeSelectOptGroup(props: ComponentProps<'optgroup'>) { return <optgroup {...props} />; }

export { NativeSelect, NativeSelectOption, NativeSelectOptGroup };
