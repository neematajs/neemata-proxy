import type { ProxyApplicationRouting, ProxyOptions } from '../dist/index.js'

const defaultRouting: ProxyApplicationRouting = { type: 'default' }
const pathRouting: ProxyApplicationRouting = { type: 'path', name: 'auth' }
const subdomainRouting: ProxyApplicationRouting = {
  type: 'subdomain',
  name: 'example.com',
}

const defaultProxyOptions: ProxyOptions = {
  listen: '127.0.0.1:0',
  applications: [{ name: 'app', routing: defaultRouting }],
  limits: {
    maxUriSize: 8192,
    maxRequestHeaders: 100,
    maxSingleHeaderSize: 8192,
    maxRequestHeaderSize: 64 * 1024,
    maxRequestBodySize: 16 * 1024 * 1024,
  },
  timeouts: {
    downstreamHeader: 10000,
    downstreamBodyIdle: 60000,
    upstreamConnect: 5000,
    upstreamRead: 60000,
    upstreamWrite: 60000,
    downstreamKeepAlive: 75,
  },
}

const proxyOptionsWithDisabledLimits: ProxyOptions = {
  listen: '127.0.0.1:0',
  applications: [{ name: 'app', routing: defaultRouting }],
  limits: null,
}

const proxyOptionsWithDisabledUriLimit: ProxyOptions = {
  listen: '127.0.0.1:0',
  applications: [{ name: 'app', routing: defaultRouting }],
  limits: {
    maxUriSize: null,
  },
}

const defaultRoutingWithName: ProxyApplicationRouting = {
  type: 'default',
  // @ts-expect-error default routing cannot carry a path/subdomain name
  name: 'unused',
}

void pathRouting
void subdomainRouting
void defaultProxyOptions
void defaultRoutingWithName
