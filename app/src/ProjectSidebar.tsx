import type { ProjectView } from "./types";

type Props = {
  projects: ProjectView[];
  onFocus: (projectRoot: string) => void;
  onClose: (projectRoot: string) => void;
  onAdd: () => void;
  view: "session" | "tools" | "settings";
  onView: (view: "session" | "tools" | "settings") => void;
};

/**
 * The project switcher.
 *
 * Deliberately shows running worker and pending-merge counts for *every* project, not
 * just the one in front: the most common way people lose work with tools like this is
 * forgetting a session is still going in a project they navigated away from.
 */
export default function ProjectSidebar({ projects, onFocus, onClose, onAdd, view, onView }: Props) {
  const elsewhere = projects
    .filter((project) => !project.active)
    .reduce(
      (total, project) => ({
        workers: total.workers + project.running_workers,
        merges: total.merges + project.pending_merges,
      }),
      { workers: 0, merges: 0 },
    );

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <h2>Projects</h2>
        <button
          className="icon-button"
          onClick={onAdd}
          aria-label="Open a project"
          title="Open a project (⌘N)"
        >
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path d="M8 3v10M3 8h10" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
          </svg>
        </button>
      </div>

      <ul className="project-list">
        {projects.map((project) => (
          <li key={project.project_root}>
            <button
              className={`project ${project.active ? "active" : ""}`}
              onClick={() => {
                onView("session");
                onFocus(project.project_root);
              }}
              title={project.project_root}
            >
              <span className="project-name">{project.name}</span>
              <span className="project-meta muted">
                {project.running_workers > 0
                  ? `${project.running_workers} running`
                  : project.live
                    ? "idle"
                    : "suspended"}
                {project.pending_merges > 0 && ` · ${project.pending_merges} to review`}
              </span>
            </button>
            <button
              className="link project-close"
              onClick={() => onClose(project.project_root)}
              aria-label={`Close ${project.name}`}
              title="Close this project"
            >
              ×
            </button>
          </li>
        ))}
      </ul>

      {(elsewhere.workers > 0 || elsewhere.merges > 0) && (
        <p className="elsewhere">
          Elsewhere:{" "}
          {elsewhere.workers > 0 && `${elsewhere.workers} worker(s) running`}
          {elsewhere.workers > 0 && elsewhere.merges > 0 && ", "}
          {elsewhere.merges > 0 && `${elsewhere.merges} diff(s) waiting`}
        </p>
      )}

      {projects.length === 0 && <p className="muted">No projects open.</p>}

      <nav className="sidebar-foot" aria-label="App">
        <button
          className={`icon-button ${view === "tools" ? "active" : ""}`}
          onClick={() => onView(view === "tools" ? "session" : "tools")}
          aria-label="Tools and skills"
          aria-pressed={view === "tools"}
          title="Tools & Skills"
        >
          {/* A wrench: what the fleet works with. */}
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path
              d="M10.5 2.2a3.3 3.3 0 0 0-3.9 4.3L2.4 10.7a1.3 1.3 0 0 0 1.9 1.9l4.2-4.2a3.3 3.3 0 0 0 4.3-3.9l-2 2-1.8-.3-.3-1.8z"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.3"
              strokeLinejoin="round"
            />
          </svg>
        </button>
        <button
          className={`icon-button ${view === "settings" ? "active" : ""}`}
          onClick={() => onView(view === "settings" ? "session" : "settings")}
          aria-label="Settings"
          aria-pressed={view === "settings"}
          title="Settings (⌘,)"
        >
          {/* Sliders, not a gear: fewer strokes to read at 14px. */}
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
            <path
              d="M3 4h10M3 8h10M3 12h10"
              stroke="currentColor"
              strokeWidth="1.3"
              strokeLinecap="round"
            />
            <circle cx="6" cy="4" r="1.6" fill="var(--panel-strong)" stroke="currentColor" strokeWidth="1.3" />
            <circle cx="10.5" cy="8" r="1.6" fill="var(--panel-strong)" stroke="currentColor" strokeWidth="1.3" />
            <circle cx="5" cy="12" r="1.6" fill="var(--panel-strong)" stroke="currentColor" strokeWidth="1.3" />
          </svg>
        </button>
      </nav>
    </aside>
  );
}
