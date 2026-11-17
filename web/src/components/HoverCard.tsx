import { ReactNode, useEffect, useRef, useState } from "react";

interface Props {
  children: ReactNode;
  card: ReactNode;
  align?: "left" | "right";
}

export function HoverCard({ children, card, align = "left" }: Props) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent | TouchEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onDocClick);
    document.addEventListener("touchstart", onDocClick);
    return () => {
      document.removeEventListener("mousedown", onDocClick);
      document.removeEventListener("touchstart", onDocClick);
    };
  }, [open]);

  return (
    <span
      ref={ref}
      className={`hover-anchor ${open ? "is-open" : ""}`}
      tabIndex={0}
      onClick={() => setOpen((v) => !v)}
      onKeyDown={(e) => {
        if (e.key === "Escape") setOpen(false);
      }}
    >
      {children}
      <span
        className={`hover-card ${align === "right" ? "align-right" : ""}`}
        role="tooltip"
        onClick={(e) => e.stopPropagation()}
      >
        {card}
      </span>
    </span>
  );
}
