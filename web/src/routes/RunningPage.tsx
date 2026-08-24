import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router";
import { api } from "../api";
import type { RunningContainer } from "../types";
import { KIND_LABEL } from "../types";

function LogsPanel({ name }: { name: string }) {
  const [logs, setLogs] = useState("");
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    api
      .containerLogs(name, 200)
      .then(setLogs)
      .catch((err: Error) => setError(err.message));
  }, [name]);

  useEffect(() => {
    load();
    const timer = setInterval(load, 3000);
    return () => clearInterval(timer);
  }, [load]);

  if (error) return <p className="error-text small">{error}</p>;
  return <pre className="logs">{logs || "(no output yet)"}</pre>;
}

export function RunningPage() {
  const [containers, setContainers] = useState<RunningContainer[] | null>(null);
  const [gpuCount, setGpuCount] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [stopping, setStopping] = useState<string | null>(null);

  const load = useCallback(() => {
    api
      .running()
      .then(setContainers)
      .catch((err: Error) => setError(err.message));
  }, []);

  useEffect(() => {
    load();
    const timer = setInterval(load, 4000);
    api.gpus().then((gpus) => setGpuCount(gpus.length)).catch(() => undefined);
    return () => clearInterval(timer);
  }, [load]);

  const stop = async (name: string) => {
    setStopping(name);
    try {
      await api.stopContainer(name);
      load();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setStopping(null);
    }
  };

  if (error) return <p className="panel error">{error}</p>;
  if (!containers) return <p className="panel">Loading running models…</p>;

  if (containers.length === 0) {
    return (
      <section className="panel empty">
        <h1>Nothing running</h1>
        <p className="muted">Launch a model from its library entry to see it here.</p>
        <Link className="button" to="/library">
          Go to library
        </Link>
      </section>
    );
  }

  return (
    <section className="stack">
      <div className="panel-head bare">
        <h1>Running</h1>
        <span className="muted">
          {containers.length} running
          {gpuCount ? ` · ${Math.min(containers.length, gpuCount)} of ${gpuCount} GPUs in use` : ""}
        </span>
      </div>
      <ul className="stack bare">
        {containers.map((c) => (
          <li className="panel" key={c.id}>
            <div className="panel-head">
              <div>
                <h3>{c.repo_id}</h3>
                <p className="muted small">
                  {KIND_LABEL[c.kind]} ·{" "}
                  {c.gpu_index !== null ? `GPU ${c.gpu_index}` : "CPU"} ·{" "}
                  {c.host_port ? (
                    <a href={`http://localhost:${c.host_port}`} target="_blank" rel="noreferrer">
                      localhost:{c.host_port}
                    </a>
                  ) : (
                    "no port mapped"
                  )}
                </p>
              </div>
              <div className="row-actions">
                <span className={`status ${c.state}`}>{c.state}</span>
                <button
                  type="button"
                  className="button danger"
                  disabled={stopping === c.name}
                  onClick={() => void stop(c.name)}
                >
                  {stopping === c.name ? "Stopping…" : "Stop"}
                </button>
              </div>
            </div>
            <button
              type="button"
              className="disclosure"
              onClick={() => setExpanded((cur) => (cur === c.name ? null : c.name))}
            >
              {expanded === c.name ? "Hide logs" : "Show logs"}
            </button>
            {expanded === c.name && <LogsPanel name={c.name} />}
          </li>
        ))}
      </ul>
    </section>
  );
}
