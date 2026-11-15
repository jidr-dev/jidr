// The PostHog proxy for site analytics (D48), a Cloudflare Pages Function:
// jidr.ly/ingest/* → PostHog's US ingestion host. Keeping the requests
// first-party means content blockers that list *.posthog.com don't drop
// them. Cookies are never forwarded either way.
const API_HOST = "us.i.posthog.com";
const ASSET_HOST = "us-assets.i.posthog.com";

export const onRequest = async ({ request }: { request: Request }) => {
  const url = new URL(request.url);
  const path = url.pathname.replace(/^\/ingest/, "") || "/";
  const upstream = new URL(path + url.search, `https://${path.startsWith("/static/") ? ASSET_HOST : API_HOST}`);

  const headers = new Headers(request.headers);
  headers.delete("cookie");
  headers.set("host", upstream.host);
  const ip = request.headers.get("cf-connecting-ip");
  if (ip) headers.set("x-forwarded-for", ip);

  const response = await fetch(upstream, {
    method: request.method,
    headers,
    body: request.method === "GET" || request.method === "HEAD" ? undefined : request.body,
  });
  const out = new Response(response.body, response);
  out.headers.delete("set-cookie");
  return out;
};
