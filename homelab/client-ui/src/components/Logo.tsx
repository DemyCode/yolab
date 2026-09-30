import { cn } from "@/lib/utils";

function Themed({
  light,
  dark,
  alt,
  className,
}: {
  light: string;
  dark: string;
  alt: string;
  className?: string;
}) {
  return (
    <>
      <img
        src={light}
        alt={alt}
        className={cn("block w-auto dark:hidden", className)}
        draggable={false}
      />
      <img
        src={dark}
        alt={alt}
        className={cn("hidden w-auto dark:block", className)}
        draggable={false}
      />
    </>
  );
}

export function Logo({ className }: { className?: string }) {
  return (
    <Themed
      light="/brand/yolab-symbol-color.svg"
      dark="/brand/yolab-symbol-reversed.svg"
      alt=""
      className={className}
    />
  );
}

export function Wordmark({ className }: { className?: string }) {
  return (
    <Themed
      light="/brand/yolab-horizontal-color.svg"
      dark="/brand/yolab-horizontal-reversed.svg"
      alt="YoLab"
      className={cn("h-9", className)}
    />
  );
}
