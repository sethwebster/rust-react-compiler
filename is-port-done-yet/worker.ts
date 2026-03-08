const GITHUB_RAW =
  "https://raw.githubusercontent.com/sethwebster/rust-react-compiler/main/AGENT-STATE.md"

interface Env {
  GITHUB_TOKEN?: string
  PUSH_SECRET?: string
  REACTIONS?: KVNamespace
  ROOM: DurableObjectNamespace
  ASSETS: Fetcher
}

interface AgentStatusPayload {
  type: "status" | "progress" | "milestone"
  message: string
  metrics?: { compileRate?: number; correctRate?: number }
  timestamp?: string
}

// ── Durable Object: Room ───────────────────────────────────────────────────────

const ALLOWED_EMOJIS = new Set(["🚀", "🔥", "❤️", "🎉", "👀", "💯"])

interface SessionMeta { id: string; country: string; city: string; x: number; y: number }

export class RoomDO implements DurableObject {
  private state: DurableObjectState
  private counts: Record<string, number> = {}
  private countsLoaded = false
  private checkedTodos: Set<string> = new Set()
  private checkedLoaded = false
  private agentStatus: AgentStatusPayload | null = null
  private agentStatusLoaded = false

  constructor(state: DurableObjectState) { this.state = state }

  async fetch(request: Request): Promise<Response> {
    // Handle non-WS push from /api/push
    if (request.method === "POST" && request.headers.get("Upgrade") !== "websocket") {
      const payload = await request.json() as AgentStatusPayload
      payload.timestamp = new Date().toISOString()
      this.agentStatus = payload
      this.state.storage.put("agent-status", payload)
      this.broadcast({ type: "agent-status", ...payload })

      // Milestone → emoji burst
      if (payload.type === "milestone") {
        const burst = ["🎉", "🚀", "🔥", "💯"]
        for (const emoji of burst) {
          this.broadcast({ type: "reaction", id: crypto.randomUUID(), emoji })
        }
      }

      return new Response(JSON.stringify({ ok: true }), {
        headers: { "Content-Type": "application/json" },
      })
    }

    if (request.headers.get("Upgrade") !== "websocket")
      return new Response("Expected WebSocket", { status: 426 })

    if (!this.countsLoaded) {
      this.counts = (await this.state.storage.get<Record<string, number>>("counts")) ?? {}
      this.countsLoaded = true
    }
    if (!this.checkedLoaded) {
      this.checkedTodos = new Set(await this.state.storage.get<string[]>("checked-todos") ?? [])
      this.checkedLoaded = true
    }

    const pair = new WebSocketPair()
    const [client, server] = Object.values(pair)

    const cf = (request as any).cf ?? {}
    const meta: SessionMeta = { id: crypto.randomUUID(), country: cf.country ?? "", city: cf.city ?? "", x: 0.5, y: 0.5 }

    // serializeAttachment persists meta with the WS — survives DO hibernation
    this.state.acceptWebSocket(server)
    server.serializeAttachment(meta)

    if (!this.agentStatusLoaded) {
      this.agentStatus = (await this.state.storage.get<AgentStatusPayload>("agent-status")) ?? null
      this.agentStatusLoaded = true
    }

    server.send(JSON.stringify({ type: "init", id: meta.id }))
    server.send(JSON.stringify({ type: "counts", counts: this.counts }))
    server.send(JSON.stringify({ type: "todo-state", checked: [...this.checkedTodos] }))
    if (this.agentStatus) {
      server.send(JSON.stringify({ type: "agent-status", ...this.agentStatus }))
    }
    this.broadcastCursors()

    return new Response(null, { status: 101, webSocket: client })
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    try {
      // Reload counts if DO woke from hibernation
      if (!this.countsLoaded) {
        this.counts = (await this.state.storage.get<Record<string, number>>("counts")) ?? {}
        this.countsLoaded = true
      }

      const msg = JSON.parse(typeof message === "string" ? message : new TextDecoder().decode(message))
      const meta: SessionMeta = ws.deserializeAttachment()
      if (!meta) return

      if (msg.type === "cursor") {
        meta.x = clamp01(msg.x)
        meta.y = clamp01(msg.y)
        ws.serializeAttachment(meta)
        this.broadcastCursors()
      } else if (msg.type === "react" && ALLOWED_EMOJIS.has(msg.emoji)) {
        this.counts[msg.emoji] = (this.counts[msg.emoji] ?? 0) + 1
        this.state.storage.put("counts", this.counts)
        this.broadcast({ type: "counts", counts: this.counts })
        // Don't echo reaction back to sender — they already spawned it optimistically
        this.broadcastExcept(ws, { type: "reaction", id: crypto.randomUUID(), emoji: msg.emoji })
      } else if (msg.type === "todo-toggle" && typeof msg.text === "string") {
        if (!this.checkedLoaded) {
          this.checkedTodos = new Set(await this.state.storage.get<string[]>("checked-todos") ?? [])
          this.checkedLoaded = true
        }
        if (this.checkedTodos.has(msg.text)) this.checkedTodos.delete(msg.text)
        else this.checkedTodos.add(msg.text)
        const checked = [...this.checkedTodos]
        this.state.storage.put("checked-todos", checked)
        this.broadcast({ type: "todo-state", checked })
      }
    } catch {}
  }

  webSocketClose(_ws: WebSocket) { this.broadcastCursors() }
  webSocketError(_ws: WebSocket) { this.broadcastCursors() }

  private broadcast(msg: object) {
    const data = JSON.stringify(msg)
    for (const ws of this.state.getWebSockets()) { try { ws.send(data) } catch {} }
  }

  private broadcastExcept(skip: WebSocket, msg: object) {
    const data = JSON.stringify(msg)
    for (const ws of this.state.getWebSockets()) { if (ws !== skip) try { ws.send(data) } catch {} }
  }

  private broadcastCursors() {
    const cursors = this.state.getWebSockets()
      .map(ws => ws.deserializeAttachment() as SessionMeta | null)
      .filter((m): m is SessionMeta => m !== null)
      .map(({ id, x, y, country, city }) => ({ id, x, y, country, city }))
    this.broadcast({ type: "cursors", cursors })
  }
}

function clamp01(n: unknown): number {
  const v = typeof n === "number" ? n : 0
  return Math.max(0, Math.min(1, v))
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

function parseTodoList(content: string): { text: string; agentDone: boolean }[] {
  const src = extractSection(content, "Todo List")
  return src
    .split("\n")
    .filter(l => /^[-*]/.test(l))
    .map(l => {
      const done = /^[-*]\s+\[x\]/i.test(l)
      const text = l.replace(/^[-*]\s+\[[ x]\]\s*/i, "").replace(/\*\*/g, "").trim()
      return text ? { text, agentDone: done } : null
    })
    .filter((x): x is { text: string; agentDone: boolean } => x !== null)
}

function parseState(content: string) {
  const s = extractSection(content, "Metrics.*?")
  const n = (rx: RegExp) => parseFloat(content.match(rx)?.[1] ?? "0") || 0
  const i = (rx: RegExp) => parseInt(content.match(rx)?.[1] ?? "0") || 0
  // Parse counts directly from fractions — never derive from percentage math
  const TOTAL_FIXTURES = 1717 // canonical fixture count — never change
  const compileMatch = s.match(/Compile rate[^(]*\((\d+)\/(\d+)/)
  const correctMatch = s.match(/Correct rate[^(]*\((\d+)\/(\d+)/)
  const metrics = {
    compileRate:      n(/Compile rate[^\d]*(\d+\.?\d*)%/),
    correctRate:      n(/Correct rate[^\d]*(\d+\.?\d*)%/),
    compileCount:     compileMatch ? parseInt(compileMatch[1]) : 0,
    correctCount:     correctMatch ? parseInt(correctMatch[1]) : 0,
    errorExpected:    i(/Error \(expected\)[^\d]*(\d+)/),
    errorUnexpected:  i(/Error \(unexpected\)[^\d]*(\d+)/),
    uncommittedFiles: i(/Uncommitted changes[^\d]*(\d+)\s*files/),
    totalFixtures:    TOTAL_FIXTURES,
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
    todoItems:            parseTodoList(content),
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

    if (pathname === "/api/push" && request.method === "POST") {
      if (env.PUSH_SECRET) {
        const auth = request.headers.get("Authorization")
        if (auth !== `Bearer ${env.PUSH_SECRET}`) {
          return Response.json({ error: "unauthorized" }, { status: 401, headers: cors })
        }
      }
      const id = env.ROOM.idFromName("main")
      const doRes = await env.ROOM.get(id).fetch(new Request("https://do/push", {
        method: "POST",
        body: await request.text(),
        headers: { "Content-Type": "application/json" },
      }))
      return new Response(doRes.body, { status: doRes.status, headers: { ...cors, "Content-Type": "application/json" } })
    }

    if (request.method === "OPTIONS") {
      return new Response(null, {
        headers: { ...cors, "Access-Control-Allow-Methods": "GET, POST, OPTIONS", "Access-Control-Allow-Headers": "Authorization, Content-Type" },
      })
    }

    if (pathname === "/api/state") {
      try {
        const state = parseState(await fetchStateFile(env.GITHUB_TOKEN))
        return Response.json(state, { headers: cors })
      } catch (e) {
        return Response.json({ error: String(e) }, { status: 500, headers: cors })
      }
    }

    if (pathname === "/api/ws") {
      const id = env.ROOM.idFromName("main")
      return env.ROOM.get(id).fetch(request)
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

    if (pathname === "/" || pathname === "") {
      try {
        const [htmlRes, state, history] = await Promise.all([
          env.ASSETS.fetch(request),
          fetchStateFile(env.GITHUB_TOKEN).then(parseState),
          fetchGitHistory(env.GITHUB_TOKEN),
        ])
        const html = await htmlRes.text()
        const script = `<script>window.__INITIAL_STATE__=${JSON.stringify(state)};window.__INITIAL_HISTORY__=${JSON.stringify(history)}</script>`
        const injected = html.replace("</head>", script + "\n</head>")
        return new Response(injected, {
          headers: { "Content-Type": "text/html;charset=UTF-8", "Cache-Control": "no-store" },
        })
      } catch {
        return env.ASSETS.fetch(request)
      }
    }

    return env.ASSETS.fetch(request)
  },
}
