import { Link } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import type { ReactNode } from "react";

/**
 * Frame around the ported operator pages.
 *
 * Storage and Backups between them encode a great deal of hard-won behaviour —
 * the Ceph state machine, the disk-removal safety checks, the restore path.
 * Rewriting that from scratch is exactly where bugs already paid for come
 * back, so it was restyled onto the new tokens and moved down a level rather
 * than redesigned. This wrapper is what gives it a way back out.
 */
export function BoxSubPage({
  title,
  subtitle,
  children,
}: {
  title: string;
  subtitle?: string;
  children: ReactNode;
}) {
  return (
    <div className="mx-auto w-full max-w-5xl px-5 py-6 md:px-8 md:py-8">
      <Link
        to="/box"
        className="mb-5 inline-flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" />
        Your box
      </Link>
      <header className="mb-6">
        <h1 className="text-2xl font-semibold tracking-tight text-fg md:text-3xl">
          {title}
        </h1>
        {subtitle && <p className="mt-1 text-sm text-fg-muted">{subtitle}</p>}
      </header>
      {children}
    </div>
  );
}
