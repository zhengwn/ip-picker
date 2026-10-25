import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { setImmediate } from 'node:timers/promises'
import test from 'node:test'
import { parse, compileScript } from '@vue/compiler-sfc'
import { build } from 'esbuild'

// Exercise App.vue's real watcher and lifecycle without mounting the UI or IPC.
const appPath = fileURLToPath(new URL('../src/App.vue', import.meta.url))
const { descriptor } = parse(await readFile(appPath, 'utf8'), { filename: appPath })
const script = compileScript(descriptor, { id: 'config-persistence-test', genDefaultAs: 'App' })
const compiled = await build({
  stdin: {
    contents: `${script.content}
      import { createRenderer } from 'vue'
      import { setInvokeHandler } from '@tauri-apps/api/core'
      export { defaultConfig }
      export function mountConfigApp(invoke) {
        setInvokeHandler(invoke)
        let state
        const component = {
          ...App,
          setup(props, context) { state = App.setup(props, context); return state },
          render() { return null },
        }
        const renderer = createRenderer({
          insert() {}, remove() {}, createElement() { return {} },
          createText() { return {} }, createComment() { return {} },
          setText() {}, setElementText() {}, parentNode() { return null },
          nextSibling() { return null }, patchProp() {},
        })
        const mounted = renderer.createApp(component)
        mounted.mount({})
        return { state, unmount() { mounted.unmount() } }
      }`,
    loader: 'ts',
    resolveDir: fileURLToPath(new URL('../src', import.meta.url)),
  },
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm',
  plugins: [{
    name: 'desktop-and-view-fixtures',
    setup(builder) {
      builder.onResolve({ filter: /\.vue$|@tauri-apps\/api\/(core|event)$|@lucide\/vue|^\.\/popovers$/ },
        args => ({ path: args.path, namespace: 'fixture' }))
      builder.onLoad({ filter: /.*/, namespace: 'fixture' }, args => ({
        contents: args.path.endsWith('/core')
          ? 'let handler; export const setInvokeHandler = value => { handler = value }; export const isTauri = () => true; export const invoke = (...args) => handler(...args);'
          : args.path.endsWith('/event')
            ? 'export const listen = async () => () => {};'
            : args.path === '@lucide/vue'
              ? [...script.content.matchAll(/import \{ ([^}]+) \} from '@lucide\/vue'/g)]
                .flatMap(match => match[1].split(',').map(name => `export const ${name.trim()} = {};`)).join('\n')
              : args.path === './popovers'
                ? 'export const dismissPopovers = () => {}; export const useDismissibleDetails = () => {};'
                : 'export default {};',
      }))
    },
  }],
})
const compiledDirectory = await mkdtemp(join(tmpdir(), 'ip-picker-config-test-'))
const compiledPath = join(compiledDirectory, 'App.config-test.mjs')
let mountConfigApp, defaultConfig
try {
  await writeFile(compiledPath, compiled.outputFiles[0].text)
  ;({ mountConfigApp, defaultConfig } = await import(pathToFileURL(compiledPath).href))
} finally { await rm(compiledDirectory, { recursive: true, force: true }) }

async function fixture(context) {
  const saves = []
  const app = mountConfigApp(async (command, args) => {
    if (command === 'load_state') return { config: defaultConfig(), favorites: { revision: 0, folders: [], entries: [] } }
    if (command === 'get_scan' || command === 'get_favorite_test') return null
    if (command === 'save_config') { saves.push(structuredClone(args.config)); return }
    throw new Error(`Unexpected command: ${command}`)
  })
  context.after(() => app.unmount())
  await setImmediate()
  assert.equal(app.state.initialized.value, true)
  assert.equal(app.state.loading.value, false)
  context.mock.timers.enable({ apis: ['setTimeout'] })
  return { ...app, saves }
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
  const { state, saves, unmount } = await fixture(context)
  state.config.value.maxTcpMs = 231
  unmount()
  context.mock.timers.tick(500)
  await setImmediate()
  assert.equal(saves.length, 0)
})
