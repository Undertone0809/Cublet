import { useEffect, useRef, useState } from "react";
import { Monitor } from "lucide-react";
import "../app-icons.css";

type AppIconProps = { path: string; name?: string };

export function AppIcon({ path, name }: AppIconProps) {
  const element = useRef<HTMLSpanElement>(null);
  const [image, setImage] = useState<{ path: string; url: string } | null>(null);
  const imageUrl = image?.path === path ? image.url : undefined;

  useEffect(() => {
    let cancelled = false;
    let requested = false;
    let observer: IntersectionObserver | undefined;
    setImage(null);
    const load = () => {
      if (requested || cancelled || !window.electronAPI) return;
      requested = true;
      observer?.disconnect();
      void window.electronAPI.invoke<string | null>("get_app_icon", { path })
        .then(url => {
          if (!cancelled && typeof url === "string" && url.length <= 128 * 1024 &&
              /^data:image\/png;base64,/.test(url)) setImage({ path, url });
        })
        .catch(() => { /* Keep the neutral fallback when system lookup fails. */ });
    };
    if (element.current && typeof IntersectionObserver !== "undefined") {
      observer = new IntersectionObserver(entries => {
        if (entries.some(entry => entry.isIntersecting)) load();
      });
      observer.observe(element.current);
    } else {
      load();
    }
    return () => {
      cancelled = true;
      observer?.disconnect();
    };
  }, [path]);

  return (
    <span ref={element} className="app-icon" aria-hidden="true" title={name}>
      {imageUrl ? (
        <img src={imageUrl} alt="" width={28} height={28} decoding="async" draggable={false}
          onError={() => setImage(current => current?.url === imageUrl ? null : current)} />
      ) : <Monitor size={22} strokeWidth={1.5} />}
    </span>
  );
}
