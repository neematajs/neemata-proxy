import http from 'node:http'
import net from 'node:net'

export async function getFreePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const server = net.createServer()
    server.unref()
    server.on('error', reject)
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address()
      if (!addr || typeof addr === 'string') {
        server.close(() => reject(new Error('Failed to get ephemeral port')))
        return
      }
      const port = addr.port
      server.close((err) => {
        if (err) reject(err)
        else resolve(port)
      })
    })
  })
}

export async function httpGet(
  port: number,
  path: string,
  headers?: Record<string, string>,
): Promise<{
  status: number
  body: string
  headers: http.IncomingHttpHeaders
}> {
  return await new Promise((resolve, reject) => {
    const req = http.request(
      { host: '127.0.0.1', port, method: 'GET', path, headers },
      (res) => {
        const chunks: Buffer[] = []
        res.on('data', (c) => chunks.push(Buffer.from(c)))
        res.on('end', () => {
          resolve({
            status: res.statusCode ?? 0,
            body: Buffer.concat(chunks).toString('utf8'),
            headers: res.headers,
          })
        })
      },
    )
    req.on('error', reject)
    req.end()
  })
}

export async function waitFor<T>(
  fn: () => Promise<T>,
  ok: (v: T) => boolean,
  timeoutMs = 5000,
) {
  const started = Date.now()
  let last: any

  while (Date.now() - started < timeoutMs) {
    try {
      const v = await fn()
      last = v
      if (ok(v)) return v
    } catch (e) {
      last = e
    }

    await new Promise((r) => setTimeout(r, 50))
  }

  throw new Error(`timeout waiting for condition; last=${String(last)}`)
}
