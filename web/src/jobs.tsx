import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import { api } from "./api";
import type { Job } from "./types";

interface JobsContextValue {
  jobs: Job[];
  connected: boolean;
  /** Bumped whenever the on-disk library changes, so pages can revalidate. */
  libraryVersion: number;
  refresh: () => Promise<void>;
}

const JobsContext = createContext<JobsContextValue | null>(null);

export function JobsProvider({ children }: { children: ReactNode }) {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [connected, setConnected] = useState(false);
  const [libraryVersion, setLibraryVersion] = useState(0);
  // Jobs the server has dropped must not be resurrected by a stale SSE frame.
  const forgotten = useRef(new Set<string>());

  const refresh = useCallback(async () => {
    const fresh = await api.jobs();
    forgotten.current = new Set();
    setJobs(fresh);
  }, []);

  useEffect(() => {
    void refresh().catch(() => undefined);

    const source = new EventSource("/api/events");
    source.onopen = () => setConnected(true);
    source.onerror = () => setConnected(false);
    source.onmessage = (event) => {
      const payload = JSON.parse(event.data) as { type: "job"; job: Job } | { type: "library" };
      if (payload.type === "library") {
        setLibraryVersion((v) => v + 1);
        return;
      }
      const job = payload.job;
      if (forgotten.current.has(job.id)) return;
      setJobs((current) => {
        const index = current.findIndex((j) => j.id === job.id);
        if (index === -1) return [...current, job];
        const next = current.slice();
        next[index] = job;
        return next;
      });
    };
    return () => source.close();
  }, [refresh]);

  const value = useMemo<JobsContextValue>(
    () => ({
      jobs: [...jobs].sort((a, b) => b.created_at - a.created_at),
      connected,
      libraryVersion,
      refresh,
    }),
    [jobs, connected, libraryVersion, refresh],
  );

  return <JobsContext.Provider value={value}>{children}</JobsContext.Provider>;
}

export function useJobs(): JobsContextValue {
  const ctx = useContext(JobsContext);
  if (!ctx) throw new Error("useJobs must be used inside <JobsProvider>");
  return ctx;
}

export function useJobActions() {
  const { refresh } = useJobs();
  return useMemo(
    () => ({
      cancel: async (id: string) => {
        await api.cancel(id);
        await refresh();
      },
      forget: async (id: string) => {
        await api.forget(id);
        await refresh();
      },
    }),
    [refresh],
  );
}

export const activeJobs = (jobs: Job[]) =>
  jobs.filter((j) => j.status === "running" || j.status === "queued");
