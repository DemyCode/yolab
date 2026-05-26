import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";
import type { ButtonHTMLAttributes } from "react";

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 rounded-md text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#a78bfa]/50 disabled:pointer-events-none disabled:opacity-50",
  {
    variants: {
      variant: {
        default: "bg-[#a78bfa] text-[#09090b] hover:bg-[#c4b5fd] font-semibold",
        outline:
          "border border-[#27272a] bg-transparent text-[#fafafa] hover:bg-[#18181b] hover:border-[#3f3f46]",
        ghost:
          "bg-transparent text-[#a1a1aa] hover:bg-[#18181b] hover:text-[#fafafa]",
        destructive:
          "border border-[#f87171]/40 bg-transparent text-[#f87171] hover:bg-[#450a0a]/30 hover:border-[#f87171]",
        secondary:
          "bg-[#18181b] border border-[#27272a] text-[#fafafa] hover:bg-[#27272a]",
      },
      size: {
        default: "h-9 px-4 py-2",
        sm: "h-7 px-3 text-xs",
        lg: "h-11 px-6 text-base",
        icon: "h-8 w-8",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

export interface ButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {}

export function Button({ className, variant, size, ...props }: ButtonProps) {
  return (
    <button
      className={cn(buttonVariants({ variant, size, className }))}
      {...props}
    />
  );
}
