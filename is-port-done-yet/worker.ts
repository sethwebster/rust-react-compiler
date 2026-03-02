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
    .filter(l => /^[-*\d]/.test(l) && !/^-{2,}$/.test(l.trim()))
    .map(l => l.replace(/^[-*•]\s+|^\d+[.)]\s*/, "").replace(/\*\*/g, "").trim())
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

function parseHistory(content: string) {
  const section = extractSection(content, "History")
  return section
    .split("\n")
    .filter(l => l.startsWith("|") && !l.includes("---") && !/^\|\s*Date\s*\|/.test(l))
    .map(row => {
      const cols = row.split("|").map(c => c.trim()).filter(Boolean)
      if (cols.length < 5) return null
      return {
        date: cols[0],
        compileRate: parseFloat(cols[1]) || 0,
        correctRate: parseFloat(cols[2]) || 0,
        overallCompletion: parseInt(cols[3]) || 0,
        passesReal: parseInt(cols[4]) || 0,
        stubs: parseInt(cols[5]) || 0,
      }
    })
    .filter(Boolean) as { date: string; compileRate: number; correctRate: number; overallCompletion: number; passesReal: number; stubs: number }[]
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
    history: parseHistory(content),
  }
}

async function fetchStateFile(token?: string): Promise<string> {
  const headers: Record<string, string> = { "Cache-Control": "no-cache" }
  if (token) headers["Authorization"] = `Bearer ${token}`
  const res = await fetch(GITHUB_RAW, { headers })
  if (!res.ok) throw new Error(`GitHub ${res.status}: ${res.statusText}`)
  return res.text()
}

// ── Git History via GraphQL (single request for all historical snapshots) ─────

interface HistoryPoint {
  date: string
  sha: string
  compileRate: number
  correctRate: number
  overallCompletion: number
  passesReal: number
  stubs: number
}

async function fetchGitHistory(token?: string): Promise<HistoryPoint[]> {
  if (!token) return []

  const query = `{
    repository(owner: "sethwebster", name: "rust-react-compiler") {
      defaultBranchRef {
        target {
          ... on Commit {
            history(path: "AGENT-STATE.md", first: 50) {
              nodes {
                committedDate
                oid
                file(path: "AGENT-STATE.md") {
                  object {
                    ... on Blob { text }
                  }
                }
              }
            }
          }
        }
      }
    }
  }`

  const res = await fetch("https://api.github.com/graphql", {
    method: "POST",
    headers: {
      "Authorization": `Bearer ${token}`,
      "Content-Type": "application/json",
      "User-Agent": "rust-react-compiler-dashboard",
    },
    body: JSON.stringify({ query }),
  })

  if (!res.ok) throw new Error(`GraphQL: ${res.status}`)
  const json = await res.json() as any
  const nodes: any[] = json?.data?.repository?.defaultBranchRef?.target?.history?.nodes ?? []

  return nodes
    .map(node => {
      try {
        const text: string = node?.file?.object?.text
        if (!text) return null
        const s = parseState(text)
        return {
          date: node.committedDate as string, // full ISO — formatted client-side
          sha: (node.oid as string).slice(0, 7),
          compileRate: s.metrics.compileRate,
          correctRate: s.metrics.correctRate,
          overallCompletion: s.overallCompletion,
          passesReal: s.passStats.real,
          stubs: s.passStats.stub,
        }
      } catch { return null }
    })
    .filter((p): p is HistoryPoint => p !== null)
    .reverse() // API returns newest-first; we want chronological
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

    if (pathname === "/api/history") {
      try {
        const history = await fetchGitHistory(env.GITHUB_TOKEN)
        return Response.json(history, {
          headers: { ...cors, "Cache-Control": "public, max-age=120" },
        })
      } catch (e) {
        return Response.json({ error: String(e) }, { status: 500, headers: cors })
      }
    }

    return env.ASSETS.fetch(request)
  },
}
