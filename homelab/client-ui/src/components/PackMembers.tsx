import { ExternalLink, Star } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { AppIconTile } from "@/components/AppIcon";
import { GitHubMark } from "@/components/GitHubMark";
import { Section } from "@/components/ui/list";
import { taglineFor } from "@/catalog/meta";
import { formatCount, githubUrl } from "@/lib/store";
import type { CatalogApp } from "@/types/apps";

const linkClass =
  "inline-flex items-center gap-1 rounded-control text-fg-muted transition-colors hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary";

export function PackMembers({ members }: { members: CatalogApp[] }) {
  const navigate = useNavigate();
  if (members.length === 0) return null;
  return (
    <Section title="Apps inside">
      {members.map((m) => {
        const repo = githubUrl(m);
        const website = m.home && m.home !== repo ? m.home : null;
        return (
          <div
            key={`${m.repo}/${m.id}`}
            className="flex items-center gap-3 px-5 py-4"
          >
            <button
              type="button"
              onClick={() => navigate(`/add/${m.id}`)}
              className="flex min-w-0 flex-1 items-center gap-3 rounded-control text-left focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary"
            >
              <AppIconTile appId={m.id} icon={m.icon} name={m.name} size="sm" />
              <span className="min-w-0">
                <span className="block text-sm font-medium text-fg">
                  {m.name}
                </span>
                <span className="block truncate text-sm text-fg-muted">
                  {taglineFor(m)}
                </span>
              </span>
            </button>
            <div className="flex shrink-0 items-center gap-4 text-sm">
              {repo && (
                <a
                  href={repo}
                  target="_blank"
                  rel="noreferrer noopener"
                  className={linkClass}
                  title={`${m.github} on GitHub`}
                >
                  <GitHubMark className="h-3.5 w-3.5" />
                  {m.stars !== null && m.stars > 0 ? (
                    <>
                      <Star className="h-3 w-3" aria-hidden />
                      <span className="font-mono tabular-nums text-fg">
                        {formatCount(m.stars)}
                      </span>
                    </>
                  ) : (
                    <span>Source</span>
                  )}
                </a>
              )}
              {website && (
                <a
                  href={website}
                  target="_blank"
                  rel="noreferrer noopener"
                  className={linkClass}
                  aria-label={`${m.name} website`}
                  title={`${m.name} website`}
                >
                  <ExternalLink className="h-3.5 w-3.5" />
                </a>
              )}
            </div>
          </div>
        );
      })}
    </Section>
  );
}
