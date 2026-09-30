import { cva } from "class-variance-authority";

export const buttonClass = cva(
  [
    "inline-flex items-center justify-center gap-2 rounded-control font-medium",
    "transition-[background-color,border-color,color,transform] duration-150",
    "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary",
    "disabled:pointer-events-none disabled:opacity-50",
    "active:scale-[0.97]",
  ].join(" "),
  {
    variants: {
      variant: {
        primary: "bg-primary text-primary-fg hover:bg-primary-deep",
        secondary:
          "border border-border-strong bg-surface text-fg hover:border-fg-subtle hover:bg-surface-2",
        outline:
          "border border-border-strong bg-transparent text-fg hover:bg-surface-2",
        ghost: "bg-transparent text-fg-muted hover:bg-surface-2 hover:text-fg",
        danger: "bg-danger text-white hover:bg-danger-deep",
        quiet:
          "bg-transparent text-danger hover:bg-danger-soft border border-transparent",
      },
      size: {
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
