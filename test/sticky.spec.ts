import http from 'node:http'

import { describe, expect, it, vi } from 'vitest'

import { Proxy as NeemataProxy } from '../dist/index.js'
import { getFreePort, httpGet, waitFor } from './_helpers'

describe('Proxy sticky sessions', () => {
  it('supports sticky sessions with cookie precedence over x-nmt-affinity-key', async () => {
    const stickyA = vi.fn()
    const stickyB = vi.fn()

    const stickyAPort = await getFreePort()
    const stickyBPort = await getFreePort()

    const serverA = http.createServer((_req, res) => {
      stickyA()
      res.statusCode = 200
      res.end('sticky-a')
    })
    const serverB = http.createServer((_req, res) => {
      stickyB()
      res.statusCode = 200
      res.end('sticky-b')
    })

    await new Promise<void>((resolve) =>
      serverA.listen(stickyAPort, '127.0.0.1', resolve),
    )
    await new Promise<void>((resolve) =>
      serverB.listen(stickyBPort, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      stickySessions: {
        enabled: true,
        cookieName: 'nmt_affinity',
        headerName: 'x-nmt-affinity-key',
        ttlMs: 600000,
      },
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: stickyAPort,
    })
    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: stickyBPort,
    })

    await proxy.start()
    try {
      const first = await httpGet(port, '/sticky', {
        'x-nmt-affinity-key': 'client-a',
      })
      expect(first.status).toBe(200)

      const setCookie = first.headers['set-cookie']
      expect(Array.isArray(setCookie) ? setCookie[0] : setCookie).toContain(
        'nmt_affinity=',
      )
      expect(Array.isArray(setCookie) ? setCookie[0] : setCookie).toContain(
        'Max-Age=600',
      )

      const cookieHeader = Array.isArray(setCookie)
        ? (setCookie[0] ?? '').split(';')[0]
        : (setCookie ?? '').split(';')[0]
      expect(cookieHeader.length).toBeGreaterThan(0)

      const second = await httpGet(port, '/sticky', {
        Cookie: cookieHeader,
        'x-nmt-affinity-key': 'conflicting-key',
      })
      expect(second.status).toBe(200)
      expect(second.body).toBe(first.body)
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) => serverA.close(() => resolve()))
      await new Promise<void>((resolve) => serverB.close(() => resolve()))
    }
  })

  it('issues affinity cookie when neither cookie nor header is provided', async () => {
    const upstreamPort = await getFreePort()
    const upstream = http.createServer((_req, res) => {
      res.statusCode = 200
      res.end('sticky-generated')
    })

    await new Promise<void>((resolve) =>
      upstream.listen(upstreamPort, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      stickySessions: { enabled: true, cookieName: 'nmt_affinity' },
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamPort,
    })

    await proxy.start()
    try {
      const first = await httpGet(port, '/sticky-generated')
      expect(first.status).toBe(200)
      expect(first.body).toBe('sticky-generated')

      const setCookie = first.headers['set-cookie']
      expect(setCookie).toBeTruthy()
      const cookieValue = Array.isArray(setCookie) ? setCookie[0] : setCookie
      expect(cookieValue).toContain('nmt_affinity=')

      const cookieHeader = (cookieValue ?? '').split(';')[0]
      const second = await httpGet(port, '/sticky-generated', {
        Cookie: cookieHeader,
      })
      expect(second.status).toBe(200)
      expect(second.body).toBe('sticky-generated')
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) => upstream.close(() => resolve()))
    }
  })

  it('uses sticky ttlMs to set cookie Max-Age', async () => {
    const upstreamPort = await getFreePort()
    const upstream = http.createServer((_req, res) => {
      res.statusCode = 200
      res.end('sticky-ttl')
    })

    await new Promise<void>((resolve) =>
      upstream.listen(upstreamPort, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      stickySessions: { enabled: true, ttlMs: 120000 },
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamPort,
    })

    await proxy.start()
    try {
      const res = await httpGet(port, '/sticky-ttl', {
        'x-nmt-affinity-key': 'ttl-check',
      })
      expect(res.status).toBe(200)

      const setCookie = res.headers['set-cookie']
      const cookieValue = Array.isArray(setCookie) ? setCookie[0] : setCookie
      expect(cookieValue).toContain('Max-Age=120')
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) => upstream.close(() => resolve()))
    }
  })

  it('ignores oversized affinity header and issues generated cookie key', async () => {
    const upstreamPort = await getFreePort()
    const upstream = http.createServer((_req, res) => {
      res.statusCode = 200
      res.end('sticky-oversized-header')
    })

    await new Promise<void>((resolve) =>
      upstream.listen(upstreamPort, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      stickySessions: { enabled: true },
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamPort,
    })

    await proxy.start()
    try {
      const oversized = 'x'.repeat(512)
      const res = await httpGet(port, '/sticky-oversized-header', {
        'x-nmt-affinity-key': oversized,
      })

      expect(res.status).toBe(200)
      const setCookie = res.headers['set-cookie']
      const cookieValue = Array.isArray(setCookie) ? setCookie[0] : setCookie
      expect(cookieValue).toContain('nmt_affinity=')
      expect(cookieValue).not.toContain(oversized)
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) => upstream.close(() => resolve()))
    }
  })

  it('ignores oversized affinity cookie and falls back to header key', async () => {
    const upstreamPort = await getFreePort()
    const upstream = http.createServer((_req, res) => {
      res.statusCode = 200
      res.end('sticky-oversized-cookie')
    })

    await new Promise<void>((resolve) =>
      upstream.listen(upstreamPort, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      stickySessions: { enabled: true },
    })

    await proxy.addUpstream('app', {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: upstreamPort,
    })

    await proxy.start()
    try {
      const oversized = 'y'.repeat(512)
      const res = await httpGet(port, '/sticky-oversized-cookie', {
        Cookie: `nmt_affinity=${oversized}`,
        'x-nmt-affinity-key': 'header-fallback',
      })

      expect(res.status).toBe(200)
      const setCookie = res.headers['set-cookie']
      const cookieValue = Array.isArray(setCookie) ? setCookie[0] : setCookie
      expect(cookieValue).toContain('nmt_affinity=header-fallback')
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) => upstream.close(() => resolve()))
    }
  })

  it('remaps sticky session when mapped upstream is removed', async () => {
    const aPort = await getFreePort()
    const bPort = await getFreePort()

    const serverA = http.createServer((_req, res) => {
      res.statusCode = 200
      res.end('sticky-remap-a')
    })
    const serverB = http.createServer((_req, res) => {
      res.statusCode = 200
      res.end('sticky-remap-b')
    })

    await new Promise<void>((resolve) =>
      serverA.listen(aPort, '127.0.0.1', resolve),
    )
    await new Promise<void>((resolve) =>
      serverB.listen(bPort, '127.0.0.1', resolve),
    )

    const port = await getFreePort()
    const proxy = new NeemataProxy({
      listen: `127.0.0.1:${port}`,
      applications: [{ name: 'app', routing: { default: true } }],
      stickySessions: { enabled: true },
      healthCheckIntervalMs: 200,
    })

    const u1 = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: aPort,
    } as const
    const u2 = {
      type: 'port',
      transport: 'http',
      secure: false,
      hostname: '127.0.0.1',
      port: bPort,
    } as const

    await proxy.addUpstream('app', u1)
    await proxy.addUpstream('app', u2)
    await proxy.start()

    try {
      const first = await httpGet(port, '/sticky-remap', {
        'x-nmt-affinity-key': 'remap-key',
      })
      expect(first.status).toBe(200)

      if (first.body === 'sticky-remap-a') {
        await proxy.removeUpstream('app', u1)
      } else {
        await proxy.removeUpstream('app', u2)
      }

      await waitFor(
        async () =>
          await httpGet(port, '/sticky-remap', {
            'x-nmt-affinity-key': 'remap-key',
          }),
        (res) => res.status === 200 && res.body !== first.body,
      )
    } finally {
      await proxy.stop()
      await new Promise<void>((resolve) =>
        serverA.close(() => resolve()),
      ).catch(() => {})
      await new Promise<void>((resolve) =>
        serverB.close(() => resolve()),
      ).catch(() => {})
    }
  })
})
