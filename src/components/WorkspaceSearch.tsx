import { Search, X } from "lucide-react";
import { useRef } from "react";
import { useI18n } from "../lib/i18n";
import { Input } from "./ui/input";

export function WorkspaceSearch({
  value, onChange, label, placeholder = label, clearLabel, disabled,
}: {
  value: string;
  onChange: (value: string) => void;
  label: string;
  placeholder?: string;
  clearLabel?: string;
  disabled?: boolean;
}) {
  const { locale } = useI18n();
  const clear = clearLabel ?? (locale === "zh" ? "清空搜索" : "Clear search");
  const input = useRef<HTMLInputElement>(null);
  return (
    <div className="compact-search">
      <Search size={14} aria-hidden="true" />
      <Input
        ref={input}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        placeholder={placeholder}
        aria-label={label}
        disabled={disabled}
        onKeyDown={(event) => {
          if (event.key === "Escape" && value) {
            event.stopPropagation();
            onChange("");
          }
        }}
      />
      {value && !disabled && (
        <button type="button" className="search-clear" aria-label={clear} title={clear} onClick={() => {
          onChange("");
          input.current?.focus();
        }}>
          <X size={13} aria-hidden="true" />
        </button>
      )}
    </div>
  );
}
