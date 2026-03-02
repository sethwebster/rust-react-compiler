// Cloudflare-compatible SSE: sends one event then closes.
// EventSource auto-reconnects using the retry interval we specify.
// Net effect: polling at 3s with SSE semantics — no long-lived connection needed.
import { fetchState, parseState } from "./_shared"

export const onRequestGet: PagesFunction<{ GITHUB_TOKEN?: string }> = async (ctx) => {
  try {
    const content = await fetchState(ctx.env.GITHUB_TOKEN)
    const state = parseState(content)
    const body = `retry: 3000\ndata: ${JSON.stringify(state)}\n\n`
    return new Response(body, {
      headers: {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-store",
        "Access-Control-Allow-Origin": "*",
      },
    })
  } catch (e) {
    const body = `retry: 5000\ndata: ${JSON.stringify({ error: String(e) })}\n\n`
    return new Response(body, {
      headers: {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-store",
        "Access-Control-Allow-Origin": "*",
      },
    })
  }
}
