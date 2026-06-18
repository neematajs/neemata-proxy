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
