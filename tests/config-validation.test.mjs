import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import { defaultConfig, validateConfig } from './support/component-fixture.mjs'

test('the settings form accepts and rejects the same boundary cases as Rust', async () => {
  const cases = JSON.parse(await readFile(new URL('./fixtures/config-validation.json', import.meta.url), 'utf8'))
  for (const { name, patch, field } of cases) {
    const issue = validateConfig({ ...defaultConfig(), ...patch })
    assert.equal(issue?.field ?? null, field, name)
    if (name === 'malformed IPv4 on the second line') assert.match(issue.message, /第 2 行/)
  }
})

test('custom lists are limited by UTF-8 bytes and count duplicate input entries', () => {
  const config = { ...defaultConfig(), source: 'custom' }
  const prefix = '192.0.2.1\n#'
  const exactSize = prefix + '中'.repeat(Math.floor((2 * 1024 * 1024 - prefix.length) / 3))
  const exactBytes = exactSize.padEnd(exactSize.length + (2 * 1024 * 1024 - Buffer.byteLength(exactSize)), ' ')
  assert.equal(validateConfig({ ...config, customIps: exactBytes }), null)
  assert.equal(validateConfig({ ...config, customIps: exactBytes + '中' })?.field, 'customIps')

  const entries = '192.0.2.1\n'.repeat(20_000)
  assert.equal(validateConfig({ ...config, customIps: entries }), null)
  assert.match(validateConfig({ ...config, customIps: entries + '192.0.2.1' }).message, /20000/)
})
