import type { RouteObject } from "react-router";
import { Layout } from "./components/Layout";
import { ErrorPage } from "./components/ErrorPage";
import { SearchPage } from "./routes/SearchPage";
import { ModelPage } from "./routes/ModelPage";
import { DownloadsPage } from "./routes/DownloadsPage";
import { LibraryPage } from "./routes/LibraryPage";
import { RunningPage } from "./routes/RunningPage";

export const routes: RouteObject[] = [
  {
    path: "/",
    element: <Layout />,
    errorElement: <ErrorPage />,
    children: [
      { index: true, element: <SearchPage /> },
      // Repo ids are `owner/name`, so the second segment is a splat.
      { path: "model/*", element: <ModelPage /> },
      { path: "downloads", element: <DownloadsPage /> },
      { path: "library", element: <LibraryPage /> },
      { path: "running", element: <RunningPage /> },
      { path: "*", element: <ErrorPage /> },
    ],
  },
];
