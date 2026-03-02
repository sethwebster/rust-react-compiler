import { fetchState, parseState } from "./_shared"

export const onRequestGet: PagesFunction<{ GITHUB_TOKEN?: string }> = async (ctx) => {
  try {
    const content = await fetchState(ctx.env.GITHUB_TOKEN)
    return Response.json(parseState(content), {
      headers: { "Access-Control-Allow-Origin": "*", "Cache-Control": "no-store" },
    })
  } catch (e) {
    return Response.json({ error: String(e) }, {
      status: 500,
      headers: { "Access-Control-Allow-Origin": "*" },
    })
  }
}
