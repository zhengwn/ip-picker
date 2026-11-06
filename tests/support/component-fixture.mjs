import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { compileScript, parse } from '@vue/compiler-sfc'
import { build } from 'esbuild'

// Exercise real Vue setup and lifecycle with a minimal renderer and mocked IPC.
const sourceDirectory = fileURLToPath(new URL('../../src/', import.meta.url))
const components = ['App.vue', 'components/FavoriteSaveDialog.vue', 'components/AllResultsPanel.vue']
const scripts = new Map(await Promise.all(components.map(async component => {
  const path = join(sourceDirectory, component)
  const { descriptor } = parse(await readFile(path, 'utf8'), { filename: path })
  return [path, compileScript(descriptor, { id: component }).content]
})))
const icons = new Set([...scripts.values()]
  .flatMap(script => [...script.matchAll(/import \{ ([^}]+) \} from '@lucide\/vue'/g)])
  .flatMap(match => match[1].split(',').map(name => name.trim())))
const { outputFiles } = await build({
  stdin: {
    contents: `
      import { createRenderer } from 'vue'
      export { default as App } from './App.vue'
      export { default as FavoriteSaveDialog } from './components/FavoriteSaveDialog.vue'
      export { default as AllResultsPanel } from './components/AllResultsPanel.vue'
      export { defaultConfig } from './types'
      export { estimateScan } from './scan-estimate'
      export { validateConfig } from './ui-state'
      export { setInvokeHandler } from '@tauri-apps/api/core'
      export { setWindowFixture } from '@tauri-apps/api/window'
      export function mountComponent(component, props = {}) {
        let state
        const renderer = createRenderer({
          insert() {}, remove() {}, createElement() { return {} },
          createText() {}, createComment() {}, setText() {}, setElementText() {},
          parentNode() { return null }, nextSibling() { return null }, patchProp() {},
        })
        const mounted = renderer.createApp({
          ...component,
          setup(props, context) { state = component.setup(props, context); return state },
          render() { return null },
        }, props)
        mounted.mount({})
        return { state, unmount() { mounted.unmount() } }
      }`,
    resolveDir: sourceDirectory,
  },
  bundle: true,
  write: false,
  platform: 'node',
  format: 'esm',
  plugins: [{
    name: 'desktop-and-view-fixtures',
    setup(builder) {
      builder.onResolve({ filter: /\.vue$/ }, args => ({ path: resolve(args.resolveDir, args.path), namespace: 'vue-setup' }))
      builder.onLoad({ filter: /.*/, namespace: 'vue-setup' }, args => ({
        contents: scripts.get(args.path) ?? 'export default {};',
        resolveDir: dirname(args.path),
        loader: 'ts',
      }))
      const modules = {
        '@tauri-apps/api/core': 'let handler; export const setInvokeHandler = value => { handler = value }; export const isTauri = () => true; export const invoke = (...args) => handler(...args);',
        '@tauri-apps/api/event': 'export const listen = async () => () => {};',
        '@tauri-apps/api/window': 'let fixture; export const setWindowFixture = value => { fixture = value }; export const getCurrentWindow = () => fixture;',
        '@lucide/vue': [...icons].map(name => `export const ${name} = {};`).join('\n'),
        'popovers': 'export const dismissPopovers = () => {}; export const useDismissibleDetails = () => {}; export const useDismissiblePopover = () => {};',
      }
      builder.onResolve({ filter: /@tauri-apps\/api\/(core|event|window)$|^@lucide\/vue$|^\.\.?(\/popovers)$/ }, args => ({
        path: args.path.endsWith('/popovers') ? 'popovers' : args.path,
        namespace: 'fixture',
      }))
      builder.onLoad({ filter: /.*/, namespace: 'fixture' }, args => ({ contents: modules[args.path] }))
    },
  }],
})

const directory = await mkdtemp(join(tmpdir(), 'ip-picker-component-test-'))
const compiledPath = join(directory, 'components.mjs')
let fixture
try {
  await writeFile(compiledPath, outputFiles[0].text)
  fixture = await import(pathToFileURL(compiledPath).href)
} finally { await rm(directory, { recursive: true, force: true }) }

export const { App, AllResultsPanel, FavoriteSaveDialog, defaultConfig, estimateScan, mountComponent, setInvokeHandler, setWindowFixture, validateConfig } = fixture
