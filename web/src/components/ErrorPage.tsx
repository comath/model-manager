import { isRouteErrorResponse, Link, useRouteError } from "react-router";

export function ErrorPage() {
  const error = useRouteError();
  const message = isRouteErrorResponse(error)
    ? `${error.status} ${error.statusText}`
    : error instanceof Error
      ? error.message
      : "This page does not exist.";

  return (
    <section className="panel empty">
      <h1>Something went sideways</h1>
      <p className="muted">{message}</p>
      <Link className="button" to="/">
        Back to discover
      </Link>
    </section>
  );
}
