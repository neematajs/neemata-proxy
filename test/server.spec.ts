import http from 'node:http'
import http2 from 'node:http2'
import net from 'node:net'

import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest'
import WebSocket, { WebSocketServer } from 'ws'

import { Proxy as NeemataProxy } from '../dist/index.js'

async function getFreePort(): Promise<number> {
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

async function httpGet(
  port: number,
  path: string,
  headers?: Record<string, string>,
): Promise<{ status: number; body: string }> {
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
          })
        })
      },
    )
    req.on('error', reject)
    req.end()
  })
}

function trackConnections(server: net.Server) {
  const sockets = new Set<net.Socket>()

  server.on('connection', (socket) => {
    sockets.add(socket)
    socket.on('close', () => {
      sockets.delete(socket)
    })
  })

  return () => {
    for (const socket of sockets) {
      socket.destroy()
    }
  }
}

async function expectRejectCode(fn: () => unknown, code: string) {
  let err: any
  try {
    const maybePromise = fn()
    await maybePromise
  } catch (e) {
    err = e
  }
  expect(err).toBeTruthy()
  expect(err).toHaveProperty('code', code)
}

async function waitFor<T>(
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

async function waitForWsFailure(
  ws: WebSocket,
  timeoutMs = 1000,
): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      ws.terminate()
      reject(new Error('timeout waiting for ws failure'))
    }, timeoutMs)

    ws.once('open', () => {
      clearTimeout(timer)
      reject(new Error('unexpected open'))
    })
    ws.once('error', () => {
      clearTimeout(timer)
      resolve()
    })
    ws.once('close', () => {
      clearTimeout(timer)
      resolve()
    })
  })
}

async function closeWs(ws: WebSocket, timeoutMs = 1000): Promise<void> {
  if (ws.readyState === WebSocket.CLOSED) return

  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => resolve(), timeoutMs)
    ws.once('close', () => {
      clearTimeout(timer)
      resolve()
    })
    try {
      ws.terminate()
    } catch {
      clearTimeout(timer)
      resolve()
    }
  })
}

describe('Proxy wiring', () => {
  let upstreamHttp1Port = 0
  let upstreamHttp1AltPort = 0
  let upstreamHttp2Port = 0
  let upstreamWsPort = 0

  let proxyHttp1Port = 0
  let proxyHttp2Port = 0
  let proxyWsPort = 0
  let proxyPathRewritePort = 0

  const toClose: Array<() => Promise<void>> = []

  beforeAll(async () => {
    // HTTP/1 upstream
    upstreamHttp1Port = await getFreePort()
    const http1 = http.createServer((req, res) => {
      res.statusCode = 200
      res.setHeader('content-type', 'text/plain')
      res.end(`h1:${req.url ?? ''}`)
    })
    const closeHttp1Sockets = trackConnections(http1)
    await new Promise<void>((resolve) =>
      http1.listen(upstreamHttp1Port, '127.0.0.1', resolve),
    )
    toClose.push(async () => {
      closeHttp1Sockets()
      await new Promise((resolve) => http1.close(() => resolve(undefined)))
    })

    // Second HTTP/1 upstream (used to verify routing/preference/dynamic update)
    upstreamHttp1AltPort = await getFreePort()
    const http1b = http.createServer((req, res) => {
      res.statusCode = 200
      res.setHeader('content-type', 'text/plain')
      res.end(`h1b:${req.url ?? ''}`)
    })
    const closeHttp1bSockets = trackConnections(http1b)
    await new Promise<void>((resolve) =>
      http1b.listen(upstreamHttp1AltPort, '127.0.0.1', resolve),
    )
    toClose.push(async () => {
      closeHttp1bSockets()
      await new Promise((resolve) => http1b.close(() => resolve(undefined)))
    })

    // HTTP/2 (h2c) upstream
    upstreamHttp2Port = await getFreePort()
    const h2 = http2.createServer()
    h2.on('stream', (stream, headers) => {
      const path = String(headers[':path'] ?? '')
      stream.respond({ ':status': 200, 'content-type': 'text/plain' })
      stream.end(`h2:${path}`)
    })
    const closeH2Sockets = trackConnections(h2)
    await new Promise<void>((resolve) =>
      h2.listen(upstreamHttp2Port, '127.0.0.1', resolve),
    )
    toClose.push(async () => {
      closeH2Sockets()
      await new Promise((resolve) => h2.close(() => resolve(undefined)))
    })

    // WebSocket upstream
    upstreamWsPort = await getFreePort()
    const wsHttp = http.createServer()
    const wss = new WebSocketServer({ server: wsHttp })
    const closeWsHttpSockets = trackConnections(wsHttp)
    wss.on('connection', (socket) => {
      socket.on('message', (msg) => {
        socket.send(`echo:${msg.toString()}`)
      })
    })
    await new Promise<void>((resolve) =>
      wsHttp.listen(upstreamWsPort, '127.0.0.1', resolve),
    )
    toClose.push(async () => {
      for (const client of wss.clients) {
        client.terminate()
      }
      await new Promise<void>((resolve) => wss.close(() => resolve()))
      closeWsHttpSockets()
      await new Promise<void>((resolve) => wsHttp.close(() => resolve()))
    })
  })

  afterAll(async () => {
    for (const close of toClose.reverse()) {
      await close()
    }
  })

  it('proxies to an HTTP/1 upstream', async () => {
    proxyHttp1Port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${proxyHttp1Port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })
    await proxy.start()
    try {
      const res = await httpGet(proxyHttp1Port, '/hello')
      expect(res.status).toBe(200)
      expect(res.body).toBe('h1:/hello')
    } finally {
      await proxy.stop()
    }
  })

  it('rejects calling start() twice with AlreadyStarted', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    await proxy.start()
    try {
      await expectRejectCode(() => proxy.start(), 'AlreadyStarted')
    } finally {
      await proxy.stop()
    }
  })

  it('concurrent start() calls do not spawn multiple listeners', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    // Fire multiple start() calls concurrently
    // Wrap each call to catch sync throws and convert to rejected promises
    const wrapStart = () => {
      try {
        return proxy.start()
      } catch (e) {
        return Promise.reject(e)
      }
    }

    const results = await Promise.allSettled([
      wrapStart(),
      wrapStart(),
      wrapStart(),
    ])

    // At least one should succeed, others may succeed or get AlreadyStarted
    const successes = results.filter((r) => r.status === 'fulfilled')
    const failures = results.filter(
      (r) => r.status === 'rejected' && r.reason?.code === 'AlreadyStarted',
    )

    // All calls should either succeed or fail with AlreadyStarted (no crashes, no bind conflicts)
    expect(successes.length + failures.length).toBe(3)
    expect(successes.length).toBeGreaterThanOrEqual(1)

    // Proxy should be functional
    const res = await httpGet(port, '/hello')
    expect(res.status).toBe(200)
    expect(res.body).toBe('h1:/hello')

    await proxy.stop()
  })

  it('stop() is idempotent (safe to call when stopped)', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    // stop before start
    await proxy.stop()

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    await proxy.start()
    await proxy.stop()
    await proxy.stop()
  })

  it('actually releases the listener port after stop()', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    await proxy.start()
    await proxy.stop()

    // Verify port is actually released: requests should fail
    await expect(httpGet(port, '/hello')).rejects.toThrow()

    // And we should be able to bind to the same port
    const testServer = net.createServer()
    await new Promise<void>((resolve, reject) => {
      testServer.once('error', reject)
      testServer.listen(port, '127.0.0.1', () => resolve())
    })
    testServer.close()
  })

  it('rejects addUpstream for unknown application with stable code', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await expectRejectCode(
      () =>
        proxy.addUpstream('nope', {
          type: 'port',
          transport: 'http',
          secure: false,
          hostname: '127.0.0.1',
          port: upstreamHttp1Port,
        }),
      'UnknownApplication',
    )
  })

  it('rejects duplicate upstreams with stable code', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    const u = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    } as const

    await proxy.addUpstream('app', u)
    await expectRejectCode(
      () => proxy.addUpstream('app', u),
      'UpstreamAlreadyExists',
    )
  })

  it('rejects removing missing upstreams with stable code', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await expectRejectCode(
      () =>
        proxy.removeUpstream('app', {
          type: 'port',
          transport: 'http',
          secure: false,
          hostname: '127.0.0.1',
          port: upstreamHttp1Port,
        }),
      'UpstreamNotFound',
    )
  })

  it('rejects unix_socket upstreams with stable code', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await expectRejectCode(
      () =>
        proxy.addUpstream('app', {
          type: 'unix_socket',
          transport: 'http',
          secure: false,
          path: '/tmp/does-not-matter.sock',
        }),
      'UnsupportedUpstreamType',
    )
  })

  it('surfaces bind failures with ListenBindFailed', async () => {
    const port = await getFreePort()
    const blocker = net.createServer()
    await new Promise<void>((resolve) =>
      blocker.listen(port, '127.0.0.1', resolve),
    )

    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    try {
      await expectRejectCode(() => proxy.start(), 'ListenBindFailed')
    } finally {
      await new Promise<void>((resolve) => blocker.close(() => resolve()))
    }
  })

  it('requires ApplicationOptions.sni for secure upstreams on default routing', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await expectRejectCode(
      () =>
        proxy.addUpstream('app', {
          type: 'port',
          transport: 'http',
          secure: true,
          hostname: '127.0.0.1',
          port: upstreamHttp1Port,
        }),
      'InvalidApplicationOptions',
    )
  })

  it('proxies to an HTTP/2 upstream (h2c)', async () => {
    proxyHttp2Port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${proxyHttp2Port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http2',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp2Port,
    })

    await proxy.start()
    try {
      const res = await httpGet(proxyHttp2Port, '/hello')
      expect(res.status).toBe(200)
      expect(res.body).toBe('h2:/hello')
    } finally {
      await proxy.stop()
    }
  })

  it('rewrites path routes by stripping the first segment', async () => {
    proxyPathRewritePort = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${proxyPathRewritePort}`,
      applications: [{ name: 'auth', routing: { type: 'path', name: 'auth' } }],
    })

    await proxy.addUpstream('auth', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    await proxy.start()
    try {
      const res = await httpGet(proxyPathRewritePort, '/auth/login?x=1')
      expect(res.status).toBe(200)
      expect(res.body).toBe('h1:/login?x=1')
    } finally {
      await proxy.stop()
    }
  })

  it('returns 500 when no default route matches', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'auth', routing: { type: 'path', name: 'auth' } }],
    })

    await proxy.addUpstream('auth', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    await proxy.start()
    try {
      const res = await httpGet(port, '/nope')
      expect(res.status).toBe(500)
    } finally {
      await proxy.stop()
    }
  })

  it('proxies WebSocket upgrades to an HTTP/1 upstream using ws', async () => {
    proxyWsPort = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${proxyWsPort}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamWsPort,
    })

    await proxy.start()
    try {
      const ws = new WebSocket(`ws://127.0.0.1:${proxyWsPort}/ws`)
      const msg = await new Promise<string>((resolve, reject) => {
        ws.on('open', () => ws.send('ping'))
        ws.on('message', (data) => resolve(data.toString()))
        ws.on('error', reject)
      })
      await closeWs(ws)

      expect(msg).toBe('echo:ping')
    } finally {
      await proxy.stop()
    }
  })

  it('routes with precedence subdomain > path', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [
        { name: 'sub', routing: { type: 'subdomain', name: 'example.com' } },
        { name: 'path', routing: { type: 'path', name: 'auth' } },
      ],
    })

    await proxy.addUpstream('sub', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1AltPort,
    })
    await proxy.addUpstream('path', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    })

    await proxy.start()
    try {
      // Host matches subdomain route; should ignore path route.
      const res1 = await httpGet(port, '/auth/login?x=1', {
        Host: 'example.com',
      })
      expect(res1.status).toBe(200)
      expect(res1.body).toBe('h1b:/auth/login?x=1')

      // Host does not match; should match path route and rewrite.
      const res2 = await httpGet(port, '/auth/login?x=1', { Host: 'other.com' })
      expect(res2.status).toBe(200)
      expect(res2.body).toBe('h1:/login?x=1')
    } finally {
      await proxy.stop()
    }
  })

  it('fails WebSocket upgrade when only an http2 pool exists', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http2',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp2Port,
    })

    await proxy.start()
    try {
      const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
      await expect(waitForWsFailure(ws)).resolves.toBeUndefined()
      await closeWs(ws)
    } finally {
      await proxy.stop()
    }
  })

  it('can swap upstreams while running (no listener restart)', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    const u1 = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    } as const
    const u2 = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1AltPort,
    } as const

    await proxy.addUpstream('app', u1)
    await proxy.start()
    try {
      const res1 = await httpGet(port, '/hello')
      expect(res1.status).toBe(200)
      expect(res1.body).toBe('h1:/hello')

      await proxy.removeUpstream('app', u1)
      await proxy.addUpstream('app', u2)

      const res2 = await httpGet(port, '/hello')
      expect(res2.status).toBe(200)
      expect(res2.body).toBe('h1b:/hello')
    } finally {
      await proxy.stop()
    }
  })

  it('supports dynamically adding and removing upstreams while running', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    const u = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamHttp1Port,
    } as const

    await proxy.start()
    try {
      // No upstreams yet => bad gateway
      await waitFor(
        async () => await httpGet(port, '/hello'),
        (r) => r.status === 500,
      )

      // Add upstream and wait for it to become usable.
      await proxy.addUpstream('app', u)
      await waitFor(
        async () => await httpGet(port, '/hello'),
        (r) => r.status === 200 && r.body === 'h1:/hello',
      )

      // Remove upstream and wait for proxy to fail again.
      await proxy.removeUpstream('app', u)
      await waitFor(
        async () => await httpGet(port, '/hello'),
        (r) => r.status === 500,
      )
    } finally {
      await proxy.stop()
    }
  })

  it('fails over to healthy upstream when one becomes unavailable', async () => {
    // Create spies to track which upstream receives requests
    const upstream1Spy = vi.fn()
    const upstream2Spy = vi.fn()

    // Create two HTTP/1 upstreams
    const upstream1Port = await getFreePort()
    const upstream2Port = await getFreePort()

    const http1a = http.createServer((req, res) => {
      upstream1Spy()
      res.statusCode = 200
      res.setHeader('content-type', 'text/plain')
      res.end('upstream1')
    })
    await new Promise<void>((resolve) =>
      http1a.listen(upstream1Port, '127.0.0.1', resolve),
    )

    const http1b = http.createServer((req, res) => {
      upstream2Spy()
      res.statusCode = 200
      res.setHeader('content-type', 'text/plain')
      res.end('upstream2')
    })
    await new Promise<void>((resolve) =>
      http1b.listen(upstream2Port, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      // Fast health checks for testing
      healthCheckIntervalMs: 200,
    })

    const u1 = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstream1Port,
    } as const
    const u2 = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstream2Port,
    } as const

    await proxy.addUpstream('app', u1)
    await proxy.addUpstream('app', u2)
    await proxy.start()

    try {
      // Wait for both upstreams to be healthy
      await waitFor(
        async () => await httpGet(port, '/hello'),
        (r) => r.status === 200,
      )

      // Both upstreams should be reachable (round-robin)
      for (let i = 0; i < 10; i++) {
        const res = await httpGet(port, '/hello')
        expect(res.status).toBe(200)
      }
      // Should have seen both upstreams
      expect(upstream1Spy).toHaveBeenCalled()
      expect(upstream2Spy).toHaveBeenCalled()

      // Shut down upstream1 and reset spies
      await new Promise<void>((resolve) => http1a.close(() => resolve()))
      upstream1Spy.mockClear()
      upstream2Spy.mockClear()

      // Wait for health check to detect the failure, then verify with spies
      // that upstream1 is never called again
      await waitFor(
        async () => {
          // Make enough requests that round-robin would statistically hit the dead upstream
          // With 2 upstreams, probability of 20 consecutive hits to one by chance is (1/2)^20
          const CONSECUTIVE_REQUESTS = 20
          for (let i = 0; i < CONSECUTIVE_REQUESTS; i++) {
            const res = await httpGet(port, '/hello')
            if (res.status !== 200) {
              // Got an error, health check hasn't kicked in yet
              return false
            }
          }
          // All requests succeeded - check if upstream1 was called
          return upstream1Spy.mock.calls.length === 0
        },
        (allToUpstream2) => allToUpstream2,
        10000, // Give health check time to detect
      )

      // Final assertion: upstream1 should never have been called after shutdown
      expect(upstream1Spy).not.toHaveBeenCalled()
      expect(upstream2Spy.mock.calls.length).toBeGreaterThanOrEqual(20)
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) => http1a.close(() => resolve())).catch(
        () => {},
      )
      await new Promise<void>((resolve) => http1b.close(() => resolve()))
    }
  })

  it('maintains existing WebSocket connections when upstream is removed', async () => {
    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
    })

    const u = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamWsPort,
    } as const

    await proxy.addUpstream('app', u)
    await proxy.start()

    try {
      // Establish WebSocket connection
      const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`)
      await new Promise<void>((resolve, reject) => {
        ws.on('open', resolve)
        ws.on('error', reject)
      })

      // Verify connection works
      const msg1 = await new Promise<string>((resolve, reject) => {
        ws.on('message', (data) => resolve(data.toString()))
        ws.on('error', reject)
        ws.send('ping1')
      })
      expect(msg1).toBe('echo:ping1')

      // Remove the upstream while connection is active
      await proxy.removeUpstream('app', u)

      // Existing connection should still work
      const msg2 = await new Promise<string>((resolve, reject) => {
        ws.once('message', (data) => resolve(data.toString()))
        ws.once('error', reject)
        ws.send('ping2')
      })
      expect(msg2).toBe('echo:ping2')

      await closeWs(ws)

      // New connections should fail (no upstreams)
      const ws2 = new WebSocket(`ws://127.0.0.1:${port}/ws`)
      await expect(waitForWsFailure(ws2)).resolves.toBeUndefined()
      await closeWs(ws2)
    } finally {
      await proxy.stop()
    }
  })
})
