import { useState, type CSSProperties, type ReactNode } from "react";
import { Layers, Link2, Monitor, Settings, ShieldCheck } from "lucide-react";
import { version } from "../../package.json";
import { BrandLogo } from "./BrandLogo";
import { WindowTitlebar } from "./WindowTitlebar";

const icons = {
  explorer: Layers,
  applications: Monitor,
  codex: ShieldCheck,
  ai: Link2,
  settings: Settings,
};
export type DesktopPage = keyof typeof icons;
const pages = Object.keys(icons) as DesktopPage[];

export function DesktopShell({ page, labels, navigationLabel, layout, onNavigate, children }: {
  page: DesktopPage;
  labels: Record<DesktopPage, string>;
  navigationLabel: string;
  layout: "canvas" | "scroll" | "embedded";
  onNavigate: (page: DesktopPage) => void;
  children: ReactNode;
}) {
  const [hover, setHover] = useState({ index: 0, visible: false, glide: false });
  const hideHover = () => {
    setHover(previous => previous.visible ? { ...previous, visible: false } : previous);
  };

  return (
    <div className="app-shell utility-shell">
      <WindowTitlebar label={labels[page]} />
      <aside className="app-sidebar">
        <div className="brand"><BrandLogo /></div>
        <nav className="app-nav" aria-label={navigationLabel} onPointerLeave={hideHover} onPointerCancel={hideHover}>
          <span
            aria-hidden="true"
            className="nav-hover"
            data-visible={hover.visible}
            data-glide={hover.glide}
            style={{ "--nav-hover-index": hover.index } as CSSProperties}
          />
          {pages.map((id, index) => {
            const Icon = icons[id];
            return (
              <button
                key={id}
                type="button"
                className={`nav-item${page === id ? " is-active" : ""}`}
                aria-current={page === id ? "page" : undefined}
                onPointerEnter={event => {
                  if (event.pointerType === "mouse") {
                    setHover(previous => ({ index, visible: true, glide: previous.visible }));
                  }
                }}
                onClick={() => onNavigate(id)}
              >
                <span className="nav-item-content">
                  <Icon size={16} aria-hidden="true" />
                  <span>{labels[id]}</span>
                </span>
              </button>
            );
          })}
        </nav>
        <div className="sidebar-footer">{version}</div>
      </aside>
      <main className="app-main">
        <div
          key={layout === "scroll" ? page : layout}
          className={layout === "embedded" ? "codex-embedded" : "utility-content"}
          data-layout={layout}
        >
          {children}
        </div>
      </main>
    </div>
  );
}
