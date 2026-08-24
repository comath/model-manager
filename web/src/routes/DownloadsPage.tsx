import { useState } from "react";
import { Link } from "react-router";
import { api, formatBytes } from "../api";
import { Progress } from "../components/Progress";
import { useJobActions, useJobs } from "../jobs";
import type { Job } from "../types";
import { KIND_LABEL } from "../types";

function JobCard({ job }: { job: Job }) {
  const { cancel, forget } = useJobActions();
  const [expanded, setExpanded] = useState(job.status === "running");
  const [error, setError] = useState<string | null>(null);
  const busy = job.status === "running" || job.status === "queued";

  const act = (fn: () => Promise<void>) => () => {
    fn().catch((err: Error) => setError(err.message));
  };

  return (
    <li className={`panel job ${job.status}`}>
      <div className="panel-head">
        <div>
          <h3>{job.repo_id}</h3>
          <p className="muted small">
            {KIND_LABEL[job.kind]} · {job.files.length} {job.files.length === 1 ? "file" : "files"} · {job.dest}
          </p>
        </div>
        <div className="row-actions">
          <span className={`status ${job.status}`}>{job.status}</span>
          {busy ? (
            <button type="button" className="button ghost" onClick={act(() => cancel(job.id))}>
              Cancel
            </button>
          ) : (
            <button type="button" className="button ghost" onClick={act(() => forget(job.id))}>
              Clear
            </button>
          )}
        </div>
      </div>

      <Progress value={job.downloaded_bytes} total={job.total_bytes} />
      <p className="muted small">
        {formatBytes(job.downloaded_bytes)} of {formatBytes(job.total_bytes)}
      </p>
      {job.error && <p className="error-text">{job.error}</p>}
      {error && <p className="error-text">{error}</p>}

      <button type="button" className="disclosure" onClick={() => setExpanded((v) => !v)}>
        {expanded ? "Hide files" : "Show files"}
      </button>
      {expanded && (
        <ul className="file-list compact">
          {job.files.map((file) => (
            <li key={file.path}>
              <div className="file">
                <span className="file-path">{file.path}</span>
                <span className={`status ${file.status}`}>{file.status}</span>
                <span className="file-size">
                  {formatBytes(file.downloaded)} / {formatBytes(file.size)}
                </span>
              </div>
              <Progress value={file.downloaded} total={file.size} />
              {file.error && <p className="error-text small">{file.error}</p>}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}

export function DownloadsPage() {
  const { jobs, refresh } = useJobs();
  const finished = jobs.filter((j) => j.status !== "running" && j.status !== "queued");

  const clearFinished = async () => {
    await Promise.allSettled(finished.map((job) => api.forget(job.id)));
    await refresh();
  };

  if (jobs.length === 0) {
    return (
      <section className="panel empty">
        <h1>No downloads yet</h1>
        <p className="muted">Find a model and pick the files you want.</p>
        <Link className="button" to="/">
          Discover models
        </Link>
      </section>
    );
  }

  return (
    <section className="stack">
      <div className="panel-head bare">
        <h1>Downloads</h1>
        {finished.length > 0 && (
          <button type="button" className="button ghost" onClick={() => void clearFinished()}>
            Clear {finished.length} finished
          </button>
        )}
      </div>
      <ul className="stack bare">
        {jobs.map((job) => (
          <JobCard key={job.id} job={job} />
        ))}
      </ul>
    </section>
  );
}
