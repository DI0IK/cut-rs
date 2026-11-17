import { ReactNode, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";

interface Props {
  children: ReactNode;
  card: ReactNode;
  align?: "left" | "right";
}

interface Pos {
  top: number;
  left: number;
}

export function HoverCard({ children, card, align = "left" }: Props) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<Pos>({ top: 0, left: 0 });
  const anchorRef = useRef<HTMLSpanElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  const closeTimer = useRef<number | null>(null);

  const cancelClose = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  };

  const scheduleClose = () => {
    cancelClose();
    closeTimer.current = window.setTimeout(() => setOpen(false), 120);
  };

  const updatePosition = useCallback(() => {
    const anchor = anchorRef.current;
    const cardEl = cardRef.current;
    if (!anchor || !cardEl) return;

    const rect = anchor.getBoundingClientRect();
    const cardRect = cardEl.getBoundingClientRect();
    const pad = 12;
    const gap = 6;

    let left = align === "right" ? rect.right - cardRect.width : rect.left;
    left = Math.max(pad, Math.min(left, window.innerWidth - cardRect.width - pad));

    let top = rect.bottom + gap;
    if (top + cardRect.height > window.innerHeight - pad && rect.top - gap - cardRect.height >= pad) {
      top = rect.top - gap - cardRect.height;
    }

    setPos({ top, left });
  }, [align]);

  useLayoutEffect(() => {
    if (open) {
      updatePosition();
    }
  }, [open, updatePosition]);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent | TouchEvent) => {
      const target = e.target as Node;
      if (
        anchorRef.current &&
        !anchorRef.current.contains(target) &&
        cardRef.current &&
        !cardRef.current.contains(target)
      ) {
        setOpen(false);
      }
    };
    const onScrollOrResize = () => updatePosition();
    document.addEventListener("mousedown", onDocClick);
    document.addEventListener("touchstart", onDocClick);
    window.addEventListener("scroll", onScrollOrResize, true);
    window.addEventListener("resize", onScrollOrResize);
    return () => {
      document.removeEventListener("mousedown", onDocClick);
      document.removeEventListener("touchstart", onDocClick);
      window.removeEventListener("scroll", onScrollOrResize, true);
      window.removeEventListener("resize", onScrollOrResize);
    };
  }, [open, updatePosition]);

  useEffect(() => () => cancelClose(), []);

  return (
    <span
      ref={anchorRef}
      className={`hover-anchor ${open ? "is-open" : ""}`}
      tabIndex={0}
      onMouseEnter={() => {
        cancelClose();
        setOpen(true);
      }}
      onMouseLeave={scheduleClose}
      onFocus={() => {
        cancelClose();
        setOpen(true);
      }}
      onBlur={scheduleClose}
      onClick={() => setOpen((v) => !v)}
      onKeyDown={(e) => {
        if (e.key === "Escape") setOpen(false);
      }}
    >
      {children}
      {open &&
        createPortal(
          <div
            ref={cardRef}
            className="hover-card is-portal-open"
            role="tooltip"
            style={{ top: pos.top, left: pos.left }}
            onMouseEnter={cancelClose}
            onMouseLeave={scheduleClose}
            onClick={(e) => e.stopPropagation()}
          >
            {card}
          </div>,
          document.body
        )}
    </span>
  );
}
