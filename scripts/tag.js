#!/usr/bin/env node

import { parseArgs } from 'node:util'

const { positionals } = parseArgs({ allowPositionals: true })
const [version] = positionals
let tag = 'latest'
const tags = { alpha: /alpha/, beta: /beta/, rc: /rc/ }
for (const [tagName, regex] of Object.entries(tags)) {
  if (regex.test(version)) {
    tag = tagName
    break
  }
}
process.stdout.write(tag)
