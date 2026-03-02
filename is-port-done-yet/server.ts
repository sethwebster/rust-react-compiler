import { serve } from "bun"
import { readFileSync } from "fs"
import { resolve, dirname } from "path"
import { fileURLToPath } from "url"

const __dirname = dirname(fileURLToPath(import.meta.url))
const STATE_FILE = resolve(__dirname, "../AGENT-STATE.md")
const HTML_FILE = resolve(__dirname, "index.html")

interface PassStatus {
  name: string
  file: string
  status: "REAL" | "PARTIAL" | "STUB"
  loc: number
}

interface AgentState {
  lastUpdated: string
  metrics: {
    compileRate: number
    correctRate: number
    errorExpected: number
    errorUnexpected: number
    uncommittedFiles: number
    totalFixtures: number
  }
  currentTask: string
  completedThisSession: string[]
  blockedOn: string[]
  nextActions: string[]
  passes: PassStatus[]
  overallCompletion: number
  passStats: { real: number; partial: number; stub: number; total: number }
}

function extractSection(content: string, headerPattern: string): string {
  const regex = new RegExp(`## ${headerPattern}\\n([\\s\\S]*?)(?=\\n## |$)`)
  const match = content.match(regex)
  return match ? match[1].trim() : ""
}

function parseMetrics(content: string): AgentState["metrics"] {
  const section = extractSection(content, "Metrics.*?")
  const compileMatch = section.match(/Compile rate[^\d]*(\d+\.?\d*)%/)
  const correctMatch = section.match(/Correct rate[^\d]*(\d+\.?\d*)%/)
  const errorExpMatch = section.match(/Error \(expected\)[^\d]*(\d+)/)
  const errorUnexpMatch = section.match(/Error \(unexpected\)[^\d]*(\d+)/)
  const uncommittedMatch = section.match(/Uncommitted changes[^\d]*(\d+)\s*files/)
  // Also check for total fixtures in compile rate line like "84.2% (1048/1244)"
  const totalMatch = section.match(/\((\d+)\/(\d+)\)/)

  return {
    compileRate: compileMatch ? parseFloat(compileMatch[1]) : 0,
    correctRate: correctMatch ? parseFloat(correctMatch[1]) : 0,
    errorExpected: errorExpMatch ? parseInt(errorExpMatch[1]) : 0,
    errorUnexpected: errorUnexpMatch ? parseInt(errorUnexpMatch[1]) : 0,
    uncommittedFiles: uncommittedMatch ? parseInt(uncommittedMatch[1]) : 0,
    totalFixtures: totalMatch ? parseInt(totalMatch[2]) : 1244,
  }
}

function parsePasses(content: string): PassStatus[] {
  const section = extractSection(content, "Pass Status Map")
  const rows = section
    .split("\n")
    .filter(l => l.startsWith("|") && !l.includes("---") && !l.match(/^\|\s*Pass\s*\|/))

  return rows
    .map(row => {
      const cols = row
        .split("|")
        .map(c => c.trim())
        .filter(Boolean)
      if (cols.length < 3) return null
      const rawStatus = cols[2].replace(/\*\*/g, "").trim()
      const status: PassStatus["status"] =
        rawStatus === "REAL" ? "REAL" : rawStatus === "PARTIAL" ? "PARTIAL" : "STUB"
      return {
        name: cols[0].replace(/\*\*/g, "").trim(),
        file: cols[1].trim(),
        status,
        loc: parseInt(cols[3]?.replace(/\*\*/g, "") ?? "0") || 0,
      }
    })
    .filter(Boolean) as PassStatus[]
}

function parseBulletList(section: string): string[] {
  return section
    .split("\n")
    .filter(l => l.match(/^[\-\*\d]/))
    .map(l =>
      l
        .replace(/^[\-\*\d]+[.)]\s*/, "")
        .replace(/\*\*/g, "")
        .trim()
    )
    .filter(Boolean)
}

function parseState(content: string): AgentState {
  const passes = parsePasses(content)
  const real = passes.filter(p => p.status === "REAL").length
  const partial = passes.filter(p => p.status === "PARTIAL").length
  const stub = passes.filter(p => p.status === "STUB").length
  const total = passes.length || 1
  const overallCompletion = Math.round(((real + partial * 0.5) / total) * 100)

  const currentTaskSection = extractSection(content, "Current Task")
  const currentTask =
    currentTaskSection
      .split("\n")
      .find(l => l.startsWith("**"))
      ?.replace(/\*\*/g, "")
      .trim() ||
    currentTaskSection.split("\n")[0]?.trim() ||
    "Unknown"

  return {
    lastUpdated: new Date().toISOString(),
    metrics: parseMetrics(content),
    currentTask,
    completedThisSession: parseBulletList(extractSection(content, "Completed This Session")),
    blockedOn: parseBulletList(extractSection(content, "Blocked On")),
    nextActions: parseBulletList(extractSection(content, "Next 3 Actions")),
    passes,
    overallCompletion,
    passStats: { real, partial, stub, total },
  }
}

const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Cache-Control": "no-cache",
}

const server = serve({
  port: 3420,
  fetch(req) {
    const url = new URL(req.url)

    if (url.pathname === "/api/state") {
      try {
        const content = readFileSync(STATE_FILE, "utf-8")
        return Response.json(parseState(content), { headers: CORS })
      } catch (e) {
        return Response.json({ error: String(e) }, { status: 500, headers: CORS })
      }
    }

    if (url.pathname === "/api/events") {
      let interval: Timer
      const stream = new ReadableStream({
        start(controller) {
          const enc = new TextEncoder()
          const send = () => {
            try {
              const content = readFileSync(STATE_FILE, "utf-8")
              const state = parseState(content)
              controller.enqueue(enc.encode(`data: ${JSON.stringify(state)}\n\n`))
            } catch (e) {
              controller.enqueue(enc.encode(`data: ${JSON.stringify({ error: String(e) })}\n\n`))
            }
          }
          send()
          interval = setInterval(send, 3000)
          req.signal.addEventListener("abort", () => clearInterval(interval))
        },
        cancel() {
          clearInterval(interval)
        },
      })
      return new Response(stream, {
        headers: {
          "Content-Type": "text/event-stream",
          ...CORS,
          Connection: "keep-alive",
        },
      })
    }

    try {
      return new Response(Bun.file(HTML_FILE))
    } catch {
      return new Response("Not found", { status: 404 })
    }
  },
})

console.log(`
┌─────────────────────────────────────────┐
│  🦀 Is the port done yet?               │
│  → http://localhost:${server.port}             │
└─────────────────────────────────────────┘
`)
