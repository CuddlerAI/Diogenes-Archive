/**
 * Brand marks. Hand-authored, unlike everything in icons.generated.tsx — these are
 * ported from website/logo.svg and website/favicon.svg, which are the authoritative
 * drawings. Do not regenerate them.
 *
 * Two cuts of the same lamp, on purpose. The site ships two for the same reason:
 * logo.svg is drawn with 2.4px strokes that collapse to mush below ~20px, so the
 * favicon is rebuilt from solid shapes. Keep them in visual sync; do not merge.
 *
 * The lamp Diogenes carried through the market at noon, looking for an honest man.
 * Its flame is the brand slash, which is why the flame stays literal green while
 * the body takes currentColor — callers tint the body to suit their ground.
 */

/** The stroked cut. For 24px and up: sidebar brand, auth card. */
export function LogoMark({ className = "logo-mark" }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 48 48" aria-hidden="true">
      <path d="M24 5v5.5" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />
      <path d="M15.5 19.5 20 11h8l4.5 8.5h-17Z" fill="currentColor" />
      <path
        d="M17 20.5h14v13.5a5 5 0 0 1-5 5h-4a5 5 0 0 1-5-5V20.5Z"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinejoin="round"
      />
      <path d="M27.4 24.4 21 34.8" stroke="var(--green)" strokeWidth="4.6" strokeLinecap="round" />
    </svg>
  );
}

/** The solid cut. For the 16px titlebar, where strokes would disappear. */
export function LogoMarkSmall() {
  return (
    <svg className="window-titlebar-mark" viewBox="0 0 32 32" aria-hidden="true">
      <rect x="14.4" y="2.5" width="3.2" height="4.5" rx="1.4" fill="currentColor" />
      <path d="M5 13.4 10.8 6h10.4L27 13.4H5Z" fill="currentColor" />
      <path d="M7 14.6h18v9.6A5.8 5.8 0 0 1 19.2 30h-6.4A5.8 5.8 0 0 1 7 24.2v-9.6Z" fill="currentColor" />
      <path d="M20 18.2 13 26" stroke="var(--green)" strokeWidth="5.4" strokeLinecap="round" />
    </svg>
  );
}
