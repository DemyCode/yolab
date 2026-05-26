import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";
import type { HTMLAttributes } from "react";

const badgeVariants = cva(
  "inline-flex items-center gap-1 rounded-full px-2.5 py-0.5 text-xs font-semibold",
  {
    variants: {
      variant: {
        default: "bg-[#a78bfa]/15 text-[#c4b5fd] border border-[#a78bfa]/30",
        success: "bg-[#4ade80]/10 text-[#4ade80] border border-[#4ade80]/25",
        warning: "bg-[#fbbf24]/10 text-[#fbbf24] border border-[#fbbf24]/25",
        destructive:
          "bg-[#f87171]/10 text-[#f87171] border border-[#f87171]/25",
        outline: "border border-[#27272a] text-[#a1a1aa]",
        muted: "bg-[#27272a] text-[#a1a1aa]",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return (
    <span className={cn(badgeVariants({ variant, className }))} {...props} />
  );
}
