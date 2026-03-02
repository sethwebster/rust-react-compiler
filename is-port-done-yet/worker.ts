const GITHUB_RAW =
  "https://raw.githubusercontent.com/sethwebster/rust-react-compiler/main/AGENT-STATE.md"

interface Env {
  GITHUB_TOKEN?: string
  ASSETS: Fetcher
}

// ── Parser ────────────────────────────────────────────────────────────────────

function extractSection(content: string, pattern: string): string {
  const rx = new RegExp(`## ${pattern}\\n([\\s\\S]*?)(?=\\n## |$)`)
  return content.match(rx)?.[1]?.trim() ?? ""
}

function parseBulletList(section: string): string[] {
  return section
    .split("\n")
    .filter(l => /^[-*\d]/.test(l))
    .map(l => l.replace(/^[-*\d]+[.)]\s*/, "").replace(/\*\*/g, "").trim())
    .filter(Boolean)
}

function parsePasses(content: string) {
  const section = extractSection(content, "Pass Status Map")
  return section
    .split("\n")
    .filter(l => l.startsWith("|") && !l.includes("---") && !/^\|\s*Pass\s*\|/.test(l))
    .map(row => {
      const cols = row.split("|").map(c => c.trim()).filter(Boolean)
      if (cols.length < 3) return null
      const raw = cols[2].replace(/\*\*/g, "").trim()
      const status = raw === "REAL" ? "REAL" : raw === "PARTIAL" ? "PARTIAL" : "STUB"
      return {
        name: cols[0].replace(/\*\*/g, "").trim(),
        file: cols[1].trim(),
        status,
        loc: parseInt(cols[3]?.replace(/\*\*/g, "") ?? "0") || 0,
      }
    })
    .filter(Boolean) as { name: string; file: string; status: "REAL" | "PARTIAL" | "STUB"; loc: number }[]
}

function parseState(content: string) {
  const s = extractSection(content, "Metrics.*?")
  const n = (rx: RegExp) => parseFloat(content.match(rx)?.[1] ?? "0") || 0
  const i = (rx: RegExp) => parseInt(content.match(rx)?.[1] ?? "0") || 0
  const totalMatch = s.match(/\((\d+)\/(\d+)\)/)
  const metrics = {
    compileRate:      n(/Compile rate[^\d]*(\d+\.?\d*)%/),
    correctRate:      n(/Correct rate[^\d]*(\d+\.?\d*)%/),
    errorExpected:    i(/Error \(expected\)[^\d]*(\d+)/),
    errorUnexpected:  i(/Error \(unexpected\)[^\d]*(\d+)/),
    uncommittedFiles: i(/Uncommitted changes[^\d]*(\d+)\s*files/),
    totalFixtures:    totalMatch ? parseInt(totalMatch[2]) : 1244,
  }

  const passes = parsePasses(content)
  const real    = passes.filter(p => p.status === "REAL").length
  const partial = passes.filter(p => p.status === "PARTIAL").length
  const stub    = passes.filter(p => p.status === "STUB").length
  const total   = passes.length || 1
  const overallCompletion = Math.round(((real + partial * 0.5) / total) * 100)

  const taskSection = extractSection(content, "Current Task")
  const currentTask =
    taskSection.split("\n").find(l => l.startsWith("**"))?.replace(/\*\*/g, "").trim() ||
    taskSection.split("\n")[0]?.trim() || "Unknown"

  return {
    lastUpdated: new Date().toISOString(),
    metrics,
    currentTask,
    completedThisSession: parseBulletList(extractSection(content, "Completed This Session")),
    blockedOn:            parseBulletList(extractSection(content, "Blocked On")),
    nextActions:          parseBulletList(extractSection(content, "Next 3 Actions")),
    passes,
    overallCompletion,
    passStats: { real, partial, stub, total },
  }
}

async function fetchStateFile(token?: string): Promise<string> {
  const headers: Record<string, string> = { "Cache-Control": "no-cache" }
  if (token) headers["Authorization"] = `Bearer ${token}`
  const res = await fetch(GITHUB_RAW, { headers })
  if (!res.ok) throw new Error(`GitHub ${res.status}: ${res.statusText}`)
  return res.text()
}

// ── Worker ────────────────────────────────────────────────────────────────────

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const { pathname } = new URL(request.url)
    const cors = { "Access-Control-Allow-Origin": "*", "Cache-Control": "no-store" }

    if (pathname === "/api/state") {
      try {
        const state = parseState(await fetchStateFile(env.GITHUB_TOKEN))
        return Response.json(state, { headers: cors })
      } catch (e) {
        return Response.json({ error: String(e) }, { status: 500, headers: cors })
      }
    }

    if (pathname === "/api/events") {
      try {
        const state = parseState(await fetchStateFile(env.GITHUB_TOKEN))
        return new Response(`retry: 3000\ndata: ${JSON.stringify(state)}\n\n`, {
          headers: { ...cors, "Content-Type": "text/event-stream" },
        })
      } catch (e) {
        return new Response(`retry: 5000\ndata: ${JSON.stringify({ error: String(e) })}\n\n`, {
          headers: { ...cors, "Content-Type": "text/event-stream" },
        })
      }
    }

    return env.ASSETS.fetch(request)
  },
}
