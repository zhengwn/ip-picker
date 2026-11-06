import assert from 'node:assert/strict'
import { setImmediate } from 'node:timers/promises'
import test from 'node:test'
import { App, defaultConfig, mountComponent, setInvokeHandler, setWindowFixture } from './support/component-fixture.mjs'

function deferred() {
  let resolve, reject
  const promise = new Promise((resolvePromise, rejectPromise) => { resolve = resolvePromise; reject = rejectPromise })
  return { promise, resolve, reject }
}

async function fixture(context, options = {}) {
  const saves = []
  let closeHandler
  const windowState = { requests: 0, destroyed: 0, unlistened: 0 }
  const windowFixture = {
    async onCloseRequested(handler) {
      closeHandler = handler
      return () => { closeHandler = undefined; windowState.unlistened++ }
    },
    async destroy() { await options.destroy?.(); windowState.destroyed++ },
  }
  // Match Tauri's wrapper: await the handler, then destroy only if not prevented.
  const requestClose = () => {
    const event = { prevented: false, preventDefault() { this.prevented = true } }
    windowState.requests++
    const completion = (async () => {
      await closeHandler?.(event)
      if (!event.prevented) await windowFixture.destroy()
    })()
    return { event, completion }
  }
  setWindowFixture(windowFixture)
  setInvokeHandler(async (command, args) => {
    if (command === 'load_state') return { config: defaultConfig(), favorites: { revision: 0, folders: [], entries: [] } }
    if (command === 'get_scan' || command === 'get_favorite_test') return null
    if (command === 'start_scan' && options.start) return options.start(args.config)
    if (command === 'save_config') {
      const snapshot = structuredClone(args.config)
      saves.push(snapshot)
      return options.save?.(snapshot, saves.length)
    }
    throw new Error(`Unexpected command: ${command}`)
  })
  const app = mountComponent(App)
  context.after(() => app.unmount())
  await setImmediate()
  assert.equal(app.state.initialized.value, true)
  assert.equal(app.state.loading.value, false)
  context.mock.timers.enable({ apis: ['setTimeout'] })
  return { ...app, saves, requestClose, windowState }
}

test('pending user settings are saved when a favorite retest starts before debounce ends', async context => {
  const { state, saves } = await fixture(context)
  state.config.value.maxTcpMs = 231
  state.favoriteTest.value = { run: { id: 'retest', status: 'running' }, favoriteIds: [] }
  assert.equal(state.locked.value, true)
  context.mock.timers.tick(500)
  await setImmediate()
  assert.equal(saves.length, 1)
  assert.equal(saves[0].maxTcpMs, 231)
  assert.equal(saves[0].source, 'cloudflare')
  assert.equal(saves[0].customIps, '')
})

test('debounced saving still coalesces edits and rejects invalid parameters', async context => {
  const { state, saves } = await fixture(context)
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(200)
  state.config.value.maxTcpMs = 232
  context.mock.timers.tick(499)
  assert.equal(saves.length, 0)
  context.mock.timers.tick(1)
  await setImmediate()
  assert.deepEqual(saves.map(config => config.maxTcpMs), [232])

  state.config.value.maxTcpMs = 0
  state.favoriteTest.value = { run: { id: 'retest', status: 'running' }, favoriteIds: [] }
  context.mock.timers.tick(500)
  await setImmediate()
  assert.equal(saves.length, 1)
})

test('unmounting clears a pending settings save', async context => {
  const { state, saves, unmount, windowState } = await fixture(context)
  state.config.value.maxTcpMs = 231
  unmount()
  context.mock.timers.tick(500)
  await setImmediate()
  assert.equal(saves.length, 0)
  assert.equal(windowState.unlistened, 1)
})

test('Done flushes the debounce and keeps the drawer open until the write succeeds', async context => {
  const write = deferred()
  const { state, saves } = await fixture(context, { save: () => write.promise })
  state.advancedOpen.value = true
  state.config.value.maxTcpMs = 231
  const finishing = state.finishSettings()
  assert.equal(saves.length, 1)
  assert.equal(saves[0].maxTcpMs, 231)
  assert.equal(state.advancedOpen.value, true)
  assert.equal(state.finishingSettings.value, true)
  state.closeSettings()
  assert.equal(state.advancedOpen.value, true)
  write.resolve()
  await finishing
  assert.equal(state.advancedOpen.value, false)
  assert.equal(state.finishingSettings.value, false)
})

test('ordinary edits made during a background write keep their own debounce', async context => {
  const firstWrite = deferred()
  const { state, saves } = await fixture(context, { save: (_, attempt) => attempt === 1 ? firstWrite.promise : undefined })
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(500)
  state.config.value.maxTcpMs = 232
  context.mock.timers.tick(200)
  state.config.value.maxTcpMs = 233
  firstWrite.resolve()
  await setImmediate()
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231])
  context.mock.timers.tick(499)
  assert.equal(saves.length, 1)
  context.mock.timers.tick(1)
  await setImmediate()
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231, 233])
})

test('failed Done retains the drawer and pending settings for a successful retry', async context => {
  const { state, saves } = await fixture(context, { save: (_, attempt) => { if (attempt === 1) throw new Error('disk full') } })
  state.advancedOpen.value = true
  state.config.value.maxTcpMs = 231
  await state.finishSettings()
  assert.equal(state.advancedOpen.value, true)
  assert.match(state.configSaveError.value, /disk full/)
  assert.equal(state.finishingSettings.value, false)
  await state.finishSettings()
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231, 231])
  assert.equal(state.advancedOpen.value, false)
  assert.equal(state.configSaveError.value, '')
})

test('closing within the debounce waits for persistence and suppresses duplicate close requests', async context => {
  const write = deferred()
  const { state, saves, requestClose, windowState } = await fixture(context, { save: () => write.promise })
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(499)
  const first = requestClose()
  const repeated = requestClose()
  assert.equal(first.event.prevented, true)
  assert.equal(repeated.event.prevented, true)
  await repeated.completion
  assert.equal(saves.length, 1)
  assert.equal(windowState.destroyed, 0)
  context.mock.timers.tick(1000)
  assert.equal(saves.length, 1)
  write.resolve()
  await first.completion
  assert.equal(windowState.destroyed, 1)
})

test('closing during a write waits for it and persists the newer queued edit before destruction', async context => {
  const firstWrite = deferred(), secondWrite = deferred()
  const { state, saves, requestClose, windowState } = await fixture(context, { save: (_, attempt) => attempt === 1 ? firstWrite.promise : secondWrite.promise })
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(500)
  assert.equal(saves.length, 1)
  state.config.value.maxTcpMs = 232
  const closing = requestClose()
  assert.equal(closing.event.prevented, true)
  assert.equal(windowState.destroyed, 0)
  firstWrite.resolve()
  await setImmediate()
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231, 232])
  assert.equal(windowState.destroyed, 0)
  secondWrite.resolve()
  await closing.completion
  assert.equal(windowState.destroyed, 1)
})

test('a save failure prevents closing and the next close retries the retained settings', async context => {
  const write = deferred()
  const { state, saves, requestClose, windowState } = await fixture(context, { save: (_, attempt) => attempt === 1 ? write.promise : undefined })
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(500)
  state.config.value.maxTcpMs = 232
  const closing = requestClose()
  write.reject(new Error('read-only disk'))
  await closing.completion
  assert.equal(windowState.destroyed, 0)
  assert.equal(state.windowClosing.value, false)
  assert.match(state.configSaveError.value, /read-only disk/)
  await requestClose().completion
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231, 232])
  assert.equal(windowState.destroyed, 1)
  assert.equal(state.configSaveError.value, '')
})

test('invalid drafts do not block closing or overwrite the most recent valid edit', async context => {
  const { state, saves, requestClose, windowState } = await fixture(context)
  state.config.value.maxTcpMs = 231
  state.config.value.maxTcpMs = 0
  assert.ok(state.validation.value)
  await requestClose().completion
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231])
  assert.equal(windowState.destroyed, 1)
})

test('closing with only an invalid draft leaves the previously saved settings intact', async context => {
  const { state, saves, requestClose, windowState } = await fixture(context)
  state.config.value.maxTcpMs = 0
  await requestClose().completion
  assert.equal(saves.length, 0)
  assert.equal(windowState.destroyed, 1)
})

test('a native field error keeps settings open but does not retry an invalid draft on close', async context => {
  const issue = { field: 'targetUrl', message: '测速地址有误' }
  const { state, saves, requestClose, windowState } = await fixture(context, { save: () => { throw issue } })
  state.advancedOpen.value = true
  state.config.value.targetUrl = 'https://example.com/download'
  await state.finishSettings()
  assert.equal(state.advancedOpen.value, true)
  assert.deepEqual(state.validationIssue.value, issue)
  assert.equal(state.configSaveError.value, '')

  await requestClose().completion
  assert.equal(windowState.destroyed, 1)
  assert.equal(saves.length, 1)
})

test('a late native rejection does not block a newer settings draft from being saved on close', async context => {
  const write = deferred()
  const { state, saves, requestClose, windowState } = await fixture(context, { save: (_, attempt) => attempt === 1 ? write.promise : undefined })
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(500)
  state.config.value.maxTcpMs = 232
  const closing = requestClose()
  write.reject({ field: 'maxTcpMs', message: '已被更新的草稿' })
  await closing.completion
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231, 232])
  assert.equal(state.validationIssue.value, null)
  assert.equal(windowState.destroyed, 1)
})

test('starting a scan displays a native validation issue on the field and editing clears it', async context => {
  const issue = { field: 'targetUrl', message: '测速地址有误' }
  const { state } = await fixture(context, { start: () => { throw issue } })
  await state.startScan()
  assert.deepEqual(state.validationIssue.value, issue)
  assert.equal(state.pageError.value, '')
  state.config.value.targetUrl = 'https://example.com/download'
  assert.equal(state.validationIssue.value, null)
})
