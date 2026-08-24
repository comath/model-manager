import { useEffect, useState } from "react";
import { NavLink, Outlet } from "react-router";
import { api } from "../api";
import { activeJobs, JobsProvider, useJobs } from "../jobs";
import type { ServerConfig } from "../types";

function Nav() {
  const { jobs, connected } = useJobs();
  const running = activeJobs(jobs).length;
  const [containerCount, setContainerCount] = useState(0);

  useEffect(() => {
    const poll = () => api.running().then((c) => setContainerCount(c.length)).catch(() => undefined);
    poll();
    const timer = setInterval(poll, 5000);
    return () => clearInterval(timer);
  }, []);

  return (
    <header className="topbar">
      <div className="brand">
        <span className="brand-mark" aria-hidden="true" />
        <span>Model Manager</span>
      </div>
      <nav>
        <NavLink to="/" end>
          Discover
        </NavLink>
        <NavLink to="/downloads">
          Downloads
          {running > 0 && <span className="badge">{running}</span>}
        </NavLink>
        <NavLink to="/library">Library</NavLink>
        <NavLink to="/running">
          Running
          {containerCount > 0 && <span className="badge">{containerCount}</span>}
        </NavLink>
      </nav>
      <span className={`link-state ${connected ? "on" : "off"}`} title="live progress stream">
        {connected ? "live" : "offline"}
      </span>
    </header>
  );
}

function Footer() {
  const [config, setConfig] = useState<ServerConfig | null>(null);
  useEffect(() => {
    api.config().then(setConfig).catch(() => undefined);
  }, []);

  if (!config) return null;
  return (
    <footer className="footer">
      <span>
        models in <code>{config.models_dir}</code>
      </span>
      <span>{config.parallel_files} parallel files</span>
      <span className={config.has_hf_token ? "ok" : "warn"}>
        {config.has_hf_token ? "HF token loaded" : "no HF token — gated repos will fail"}
      </span>
    </footer>
  );
}

export function Layout() {
  return (
    <JobsProvider>
      <div className="shell">
        <Nav />
        <main>
          <Outlet />
        </main>
        <Footer />
      </div>
    </JobsProvider>
  );
}
