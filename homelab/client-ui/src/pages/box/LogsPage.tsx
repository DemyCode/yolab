import { useMemo, useState } from "react";
import { AlertTriangle, RefreshCw, Search, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { useApi } from "@/lib/useResource";
import { cn } from "@/lib/utils";

interface LogEntry {
  timestamp: string;
  unit: string;
  priority: number;
  message: string;
}

interface LogsResponse {
  entries: LogEntry[];
  units: string[];
  truncated: boolean;
}

/**
 * journald severities. Only the three thresholds anyone actually wants are
 * offered — the eight-level syslog scale is an implementation detail, and
 * "Errors only / Warnings and worse / Everything" is the real question.
 */
const LEVELS = [
  { label: "Everything", priority: undefined },
  { label: "Warnings", priority: 4 },
  { label: "Errors", priority: 3 },
] as const;

const RANGES = [
  { label: "This boot", since: undefined },
  { label: "Last hour", since: "1 hour ago" },
  { label: "Today", since: "today" },
  { label: "Last 7 days", since: "7 days ago" },
] as const;

/** Colour by severity, so a wall of text has shape before it is read. */
function toneOf(priority: number): string {
  if (priority <= 3) return "text-danger";
  if (priority === 4) return "text-warning";
  return "text-fg-muted";
}

/**
 * Rendered in the viewer's timezone, not the machine's. Correlating a log line
 * with "the thing I just did" is the main use, and that happens in local time.
 */
function formatTime(iso: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  return Number.isNaN(d.getTime())
    ? ""
    : d.toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      });
}

/** Unit names are long and repetitive; the suffix carries no information. */
function shortUnit(unit: string): string {
  return unit.replace(/\.service$/, "");
}

/**
 * Everything the machine has said, in one place.
 *
 * This exists because the two worst failures this platform has had were both
 * plainly visible in the journal for hours while nobody could see them without
 * SSH: a storage unit that silently could not log at all, and a timer that had
 * stopped firing days earlier. The fix for both was obvious once read.
 *
 * Deliberately not a live tail. A page that scrolls on its own is unusable for
 * the thing people actually do here — find the moment something broke and read
 * around it — and polling a full journal query every second is expensive on a
 * machine that may already be struggling. Refresh is a button.
 */
export function LogsPage() {
  const [level, setLevel] = useState(0);
  const [range, setRange] = useState(0);
  const [unit, setUnit] = useState("");
  const [search, setSearch] = useState("");

  const query = useMemo(() => {
    const p = new URLSearchParams();
    const priority = LEVELS[level].priority;
    const since = RANGES[range].since;
    if (priority !== undefined) p.set("priority", String(priority));
    if (since) p.set("since", since);
    if (unit) p.set("unit", unit);
    if (search.trim()) p.set("search", search.trim());
    p.set("limit", "500");
    return p.toString();
  }, [level, range, unit, search]);

  const logs = useApi<LogsResponse>(`logs?${query}`, `/api/logs?${query}`);

  const entries = logs.data?.entries ?? [];
  // Offered from what the journal actually contains, so the filter can never
  // list a unit with nothing behind it.
  const units = logs.data?.units ?? [];

  return (
    <div className="space-y-4">
      <Card className="space-y-3 p-4">
        <div className="relative">
          <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-fg-subtle" />
          <Input
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search the messages"
            className="pl-9 pr-9"
          />
          {search && (
            <button
              onClick={() => setSearch("")}
              aria-label="Clear search"
              className="absolute right-3 top-1/2 -translate-y-1/2 text-fg-subtle hover:text-fg"
            >
              <X className="size-4" />
            </button>
          )}
        </div>

        <div className="flex flex-wrap gap-2">
          <SegmentedControl
            options={LEVELS.map((l) => l.label)}
            value={level}
            onChange={setLevel}
          />
          <SegmentedControl
            options={RANGES.map((r) => r.label)}
            value={range}
            onChange={setRange}
          />
        </div>

        <div className="flex items-center gap-2">
          <select
            value={unit}
            onChange={(e) => setUnit(e.target.value)}
            className="min-w-0 flex-1 rounded-xl border border-line bg-surface px-3 py-2 text-sm text-fg"
          >
            <option value="">Everything on this machine</option>
            {units.map((u) => (
              <option key={u} value={u}>
                {shortUnit(u)}
              </option>
            ))}
          </select>
          <Button
            variant="secondary"
            onClick={() => logs.refresh()}
            aria-label="Refresh"
          >
            <RefreshCw
              className={cn("size-4", logs.loading && "animate-spin")}
            />
          </Button>
        </div>
      </Card>

      {logs.error && (
        <Banner tone="error" title="Could not read the logs">
          {String(logs.error)}
        </Banner>
      )}

      {logs.data?.truncated && (
        <Banner tone="info" title="Showing the most recent lines only">
          There were more than 500 matching lines. Narrow the time range or
          search to see the rest.
        </Banner>
      )}

      <Card className="overflow-hidden p-0">
        {logs.loading && !logs.data ? (
          <div className="space-y-2 p-4">
            {Array.from({ length: 8 }).map((_, i) => (
              <Skeleton key={i} className="h-4 w-full" />
            ))}
          </div>
        ) : entries.length === 0 ? (
          <div className="flex flex-col items-center gap-2 p-10 text-center">
            <AlertTriangle className="size-5 text-fg-subtle" />
            <div className="text-sm text-fg-muted">
              Nothing matches these filters.
            </div>
            <div className="text-xs text-fg-subtle">
              Try a wider time range, or “Everything” instead of errors only.
            </div>
          </div>
        ) : (
          <div className="max-h-[65vh] overflow-auto">
            {entries.map((e, i) => (
              <div
                key={`${e.timestamp}-${i}`}
                className="flex gap-3 border-b border-line/50 px-4 py-2 font-mono text-xs last:border-0"
              >
                <span className="shrink-0 tabular-nums text-fg-subtle">
                  {formatTime(e.timestamp)}
                </span>
                <span className="w-40 shrink-0 truncate text-fg-subtle">
                  {shortUnit(e.unit)}
                </span>
                {/* break-all, not truncate: a log line that has been cut off is
                    the one thing a log page must never do. */}
                <span className={cn("min-w-0 break-all", toneOf(e.priority))}>
                  {e.message}
                </span>
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  );
}

function SegmentedControl({
  options,
  value,
  onChange,
}: {
  options: readonly string[];
  value: number;
  onChange: (i: number) => void;
}) {
  return (
    <div className="flex gap-1 rounded-xl bg-surface-2 p-1">
      {options.map((label, i) => (
        <button
          key={label}
          onClick={() => onChange(i)}
          className={cn(
            "rounded-lg px-3 py-1.5 text-sm transition-colors",
            value === i
              ? "bg-surface font-medium text-fg shadow-[var(--shadow-card)]"
              : "text-fg-muted hover:text-fg",
          )}
        >
          {label}
        </button>
      ))}
    </div>
  );
}
