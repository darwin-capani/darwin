import type { ReactNode } from "react";

/**
 * Shared FUI panel chrome (restyle contract item 4): an angular cut-corner
 * frame (octagonal clip-path stroke), four corner brackets, and an optional
 * beveled-trapezoid title bar with a leading light-streak edge plus tick
 * decorations. Purely presentational — all data stays in the children.
 */
export default function Frame({
  className = "",
  title,
  tag,
  children,
}: {
  className?: string;
  title?: ReactNode;
  tag?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className={`frame ${className}`}>
      <div className="frame-clip">
        <div className="frame-inner">
          {title !== undefined && (
            <div className="frame-title">
              <span className="t">{title}</span>
              {tag !== undefined && <span className="tag">{tag}</span>}
              <span className="ticks" aria-hidden="true">
                <i />
                <i />
                <i />
              </span>
            </div>
          )}
          {children}
        </div>
      </div>
      <i className="bk tl" aria-hidden="true" />
      <i className="bk tr" aria-hidden="true" />
      <i className="bk bl" aria-hidden="true" />
      <i className="bk br" aria-hidden="true" />
    </section>
  );
}
