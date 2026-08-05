import { cva } from "class-variance-authority";

/**
 * Button styling, kept in its own module so `button.tsx` exports only
 * components — otherwise React Fast Refresh gives up on the whole file.
 *
 * Router links import this directly: a control that navigates should still be
 * an `<a>` so that middle-click, "open in new tab" and screen readers all
 * behave, and it borrows the classes rather than being wrapped in a `<button>`.
 */
export const buttonClass = cva(
  [
    "inline-flex items-center justify-center gap-2 rounded-xl font-medium",
    "transition-[background-color,border-color,color,transform] duration-150",
    "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/50 focus-visible:ring-offset-2 focus-visible:ring-offset-bg",
    "disabled:pointer-events-none disabled:opacity-50",
    // A press state that actually responds. On touch this is most of what
    // makes a web app feel like an app.
    "active:scale-[0.97]",
  ].join(" "),
  {
    variants: {
      variant: {
        primary: "bg-primary text-primary-fg hover:brightness-110",
        secondary:
          "bg-surface-2 text-fg hover:bg-surface-3 border border-border",
        outline:
          "border border-border-strong bg-transparent text-fg hover:bg-surface-2",
        ghost: "bg-transparent text-fg-muted hover:bg-surface-2 hover:text-fg",
        danger: "bg-danger text-white hover:brightness-110",
        quiet:
          "bg-transparent text-danger hover:bg-danger-soft border border-transparent",
      },
      size: {
        // 44px: the smallest reliable touch target. The old UI used 28-36px
        // controls throughout, which is fine with a mouse and awful with a
        // thumb.
        md: "h-11 px-4 text-sm",
        sm: "h-9 px-3 text-sm",
        lg: "h-12 px-6 text-base",
        icon: "h-11 w-11",
        "icon-sm": "h-9 w-9",
      },
      full: { true: "w-full", false: "" },
    },
    defaultVariants: { variant: "primary", size: "md", full: false },
  },
);
