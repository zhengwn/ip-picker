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
    if (command === 'start_scan' && options.start) return options.start(args.config, args.persistConfig)
    if (command === 'prepare_shutdown') return options.prepareShutdown?.()
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

test('shared latency selection stays aligned even before a scan result table exists', async context => {
  const { state } = await fixture(context)
  assert.equal(state.run.value, null)
  state.resultPreferences.value.tcpDisplayMetric = 'average'
  assert.equal(state.resultPreferences.value.sortBy, 'averageTcp')
  state.resultPreferences.value.sortBy = 'peakTcp'
  assert.equal(state.resultPreferences.value.tcpDisplayMetric, 'peak')
  state.resultPreferences.value.sortBy = 'download'
  state.resultPreferences.value.tcpDisplayMetric = 'median'
  assert.equal(state.resultPreferences.value.sortBy, 'download')
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

test('a due autosave of a newer edit proceeds after an older background write fails', async context => {
  const firstWrite = deferred()
  const { state, saves } = await fixture(context, { save: (_, attempt) => attempt === 1 ? firstWrite.promise : undefined })
  state.config.value.maxTcpMs = 231
  context.mock.timers.tick(500)
  state.config.value.maxTcpMs = 232
  context.mock.timers.tick(500)
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231])
  firstWrite.reject(new Error('temporary storage failure'))
  await setImmediate()
  assert.deepEqual(saves.map(config => config.maxTcpMs), [231, 232])
  assert.equal(state.configSaveError.value, '')
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

test('closing waits for the final measurement save and allows retry if it fails', async context => {
  const write = deferred()
  let attempts = 0
  const { state, requestClose, windowState } = await fixture(context, {
    prepareShutdown: () => ++attempts === 1 ? write.promise : undefined,
  })
  const closing = requestClose()
  await setImmediate()
  assert.equal(attempts, 1)
  assert.equal(windowState.destroyed, 0)
  write.reject('测速结果无法保存')
  await closing.completion
  assert.equal(windowState.destroyed, 0)
  assert.equal(state.windowClosing.value, false)
  assert.match(state.closeError.value, /测速结果无法保存/)

  await requestClose().completion
  assert.equal(attempts, 2)
  assert.equal(windowState.destroyed, 1)
  assert.equal(state.closeError.value, '')
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

test('quick validation preserves an explicitly enabled budget and saves pending ordinary settings', async context => {
  const write = deferred()
  const starts = []
  const { state, saves } = await fixture(context, {
    save: () => write.promise,
    start(config, persistConfig) {
      starts.push({ config, persistConfig })
      return { id: 'quick', quickValidation: !persistConfig, status: 'running', config, results: [] }
    },
  })
  state.config.value.maxCandidates = 2000
  state.config.value.regions = ['JP']
  state.config.value.downloadBudgetMiB = 32
  state.config.value.downloadBudgetEnabled = true

  const starting = state.startScan(true)
  assert.equal(starts.length, 0)
  write.resolve()
  await starting
  assert.equal(saves.at(-1).maxCandidates, 2000)
  assert.equal(starts[0].persistConfig, false)
  const quick = starts[0].config
  assert.deepEqual(quick.regions, ['JP'])
  assert.equal(quick.maxCandidates, 256)
  assert.equal(quick.regionalCandidateLimit, 4)
  assert.equal(quick.stabilityAttempts, 10)
  assert.equal(quick.downloadBudgetEnabled, true)
  assert.equal(quick.downloadBudgetMiB, 32)
  assert.equal(state.run.value.config.maxCandidates, 256)
  assert.equal(state.config.value.maxCandidates, 2000)
  assert.equal(state.config.value.stabilityAttempts, 100)
  assert.equal(state.dirty.value, false, 'quick mode overrides are not user edits')
  state.config.value.maxTcpMs = 300
  assert.equal(state.dirty.value, true, 'a real change still prompts for the next run')
})

test('quick validation defaults to no budget and never caps an explicit budget at 64 MiB', async context => {
  const starts = []
  const { state } = await fixture(context, {
    start(config, persistConfig) {
      starts.push({ config, persistConfig })
      return { id: `run-${starts.length}`, quickValidation: !persistConfig, status: 'completed', config, results: [] }
    },
  })
  assert.equal(state.quickEstimate.value.download, '约 763 MiB')
  await state.startScan(true)
  assert.equal(starts[0].config.downloadBudgetEnabled, false)
  assert.equal(state.dirty.value, false)

  state.config.value.downloadBudgetEnabled = true
  state.config.value.downloadBudgetMiB = 512
  assert.equal(state.quickEstimate.value.download, '最多 512 MiB')
  await state.startScan(true)
  assert.equal(starts[1].config.downloadBudgetEnabled, true)
  assert.equal(starts[1].config.downloadBudgetMiB, 512)

  state.config.value.downloadBudgetEnabled = false
  await state.startScan(true)
  await state.startScan()
  assert.equal(starts[2].config.downloadBudgetEnabled, false)
  assert.equal(starts[3].config.downloadBudgetEnabled, false)
  assert.equal(starts[3].config.stabilityAttempts, 100)
})

test('native config key order never creates a false settings-changed warning after restoring defaults', async context => {
  let index = 0
  const { state } = await fixture(context, {
    start(config, persistConfig) {
      return {
        id: `reordered-${++index}`, quickValidation: !persistConfig, status: 'completed', results: [],
        config: Object.fromEntries(Object.entries(config).reverse()),
      }
    },
  })
  state.resetParameters()
  await state.startScan()
  assert.equal(state.dirty.value, false)
  await state.startScan(true)
  assert.equal(state.dirty.value, false)
  state.config.value.regions = ['HK']
  assert.equal(state.dirty.value, true)
})
