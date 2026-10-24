<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { Check, CircleAlert, LoaderCircle, Plus, Star, X } from '@lucide/vue'
import { regionName, type IpResult, type ScanRun } from '../types'
import { candidateMeasurementStatus, targetHost } from '../ui-state'
import { favoriteDate, favoriteError, favoriteNumber, favoriteTcpSummary, type FavoriteLibrary } from '../favorites'
import SelectControl from './SelectControl.vue'

const props = defineProps<{ library: FavoriteLibrary; run: ScanRun; result: IpResult; desktop: boolean; open: boolean }>()
const emit = defineEmits<{
  saved: [library: FavoriteLibrary]
  change: [library: FavoriteLibrary]
  close: []
  notify: [text: string, error?: boolean]
}>()

const dialog = ref<HTMLDialogElement | null>(null)
const folderSelect = ref<HTMLSelectElement | null>(null)
const folderNameInput = ref<HTMLInputElement | null>(null)
const folderId = ref(props.library.folders[0]?.id ?? '')
const note = ref('')
const newFolderOpen = ref(!props.library.folders.length)
const newFolderName = ref('')
const pending = ref<'save' | 'folder' | null>(null)
const error = ref('')
const folderStatus = ref('')
let returnFocus: HTMLElement | null = null

const location = computed(() => /^[A-Z]{2}$/.test(props.result.regionCode) ? regionName(props.result.regionCode) : '未知地区')
const tcpSummary = computed(() => favoriteTcpSummary(props.result, props.run.config))
const existing = computed(() => props.library.entries.find(entry => entry.folderId === folderId.value && entry.result.ip === props.result.ip))
const validFolder = computed(() => props.library.folders.some(folder => folder.id === folderId.value))
const invalidFolderName = computed(() => !newFolderName.value.trim() || newFolderName.value.trim().length > 40 || props.library.folders.some(folder => folder.name === newFolderName.value.trim()))
const saveDisabled = computed(() => !props.desktop || !!pending.value || !validFolder.value || note.value.length > 500)

watch(folderId, () => { note.value = existing.value?.note ?? ''; folderStatus.value = '' }, { immediate: true, flush: 'sync' })
watch(() => props.library.folders, folders => {
  if (!folders.some(folder => folder.id === folderId.value)) folderId.value = folders[0]?.id ?? ''
  if (!folders.length) newFolderOpen.value = true
})
watch(() => props.open, async open => {
  await nextTick()
  if (open) show()
  else dialog.value?.close()
})

function show() {
  if (!dialog.value || dialog.value.open || !props.open) return
  returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
  dialog.value.showModal()
  if (props.library.folders.length) folderSelect.value?.focus()
  else folderNameInput.value?.focus()
}

function close() {
  if (pending.value) return
  dialog.value?.close()
  emit('close')
  restoreFocus()
}

function restoreFocus() {
  const target = returnFocus
  returnFocus = null
  if (!target?.isConnected || target.matches(':disabled')) return
  const active = document.activeElement
  if (active === target) return
  // Native dialog restoration or the parent may already have chosen a new target.
  if (active instanceof HTMLElement && active !== document.body && active !== dialog.value && !dialog.value?.contains(active)) return
  target.focus({ preventScroll: true })
}

function onBackdropClick(event: MouseEvent) {
  if (event.target === dialog.value) close()
}

async function toggleNewFolder() {
  folderStatus.value = ''
  newFolderOpen.value = !newFolderOpen.value
  if (newFolderOpen.value) { await nextTick(); folderNameInput.value?.focus() }
}

async function createFolder() {
  if (!props.desktop || pending.value || invalidFolderName.value) return
  pending.value = 'folder'
  error.value = ''
  folderStatus.value = ''
  const preservedNote = note.value
  const name = newFolderName.value.trim()
  try {
    const library = await invoke<FavoriteLibrary>('create_favorite_folder', { name })
    emit('change', library)
    // The response is authoritative; select the newly created folder after props update.
    await nextTick()
    folderId.value = library.folders.find(folder => folder.name === name)?.id ?? library.folders[0]?.id ?? ''
    note.value = preservedNote
    newFolderName.value = ''
    newFolderOpen.value = false
    folderStatus.value = `已新建并选中「${name}」`
    emit('notify', '已新建收藏夹')
    pending.value = null
    await nextTick()
    folderSelect.value?.focus()
  } catch (cause) { error.value = favoriteError(cause) }
  finally { pending.value = null }
}

async function save() {
  if (saveDisabled.value) return
  const updating = !!existing.value
  pending.value = 'save'
  error.value = ''
  try {
    const library = await invoke<FavoriteLibrary>('save_favorite', { folderId: folderId.value, runId: props.run.id, ip: props.result.ip, note: note.value.trim() })
    emit('saved', library)
    emit('notify', updating ? '已更新收藏中的本次测试快照' : '已收藏 IP 和本次测试快照')
    pending.value = null
    close()
  } catch (cause) { error.value = favoriteError(cause) }
  finally { pending.value = null }
}

onMounted(show)
onBeforeUnmount(() => { dialog.value?.close(); restoreFocus() })
</script>

<template>
  <Teleport to="body">
    <dialog ref="dialog" class="favorite-save-dialog" aria-labelledby="favorite-save-title" aria-describedby="favorite-snapshot-explanation" @cancel.prevent="close" @click="onBackdropClick">
      <form @submit.prevent="save">
        <div class="favorite-save-heading"><div class="favorite-heading-title"><span class="favorite-heading-icon"><Star :size="16" aria-hidden="true" /></span><h2 id="favorite-save-title">收藏测试结果</h2></div><button type="button" class="icon-button" :disabled="!!pending" aria-label="关闭收藏窗口" @click="close"><X :size="17" aria-hidden="true" /></button></div>
        <div class="favorite-save-body">
          <div class="favorite-save-preview">
            <div class="favorite-preview-identity"><strong class="mono">{{ result.ip }}</strong><span class="favorite-preview-location" :title="result.city || undefined">{{ location }}<span v-if="result.colo" class="favorite-preview-pop">{{ result.colo }}</span></span></div>
            <p class="favorite-preview-status">{{ candidateMeasurementStatus(result, run) }}</p>
            <div class="favorite-save-metrics"><span><span>TCP {{ tcpSummary.label }}</span><strong>{{ favoriteNumber(tcpSummary.value) }} <small>ms</small></strong></span><span><span>下载速度</span><strong>{{ result.downloadSamplesMbps.length ? favoriteNumber(result.downloadMbps) : '—' }} <small>Mbps</small></strong></span></div>
            <div class="favorite-save-meta"><span :title="run.config.targetUrl">{{ targetHost(run.config.targetUrl) }}</span><time :datetime="run.startedAt" :title="`测试开始于 ${favoriteDate(run.startedAt)}`">{{ favoriteDate(run.startedAt) }}</time></div>
          </div>
          <p id="favorite-snapshot-explanation" class="favorite-save-explanation">{{ run.status === 'running' ? '测试进行中，保存此刻已取得的数据、时间、目标与参数。' : '保存此 IP 的测试数据、时间、目标与本次参数。' }}后续测试不会自动覆盖此快照。</p>
          <p v-if="!desktop" class="favorite-save-error" role="status">浏览器预览不保存收藏，请在桌面应用中使用。</p>
          <div class="favorite-save-folder-label"><label for="favorite-save-folder">收藏到</label><button v-if="library.folders.length" class="text-button" type="button" :disabled="!!pending || !desktop" :aria-expanded="newFolderOpen" aria-controls="favorite-new-folder" @click="toggleNewFolder"><Plus :size="12" aria-hidden="true" />新建收藏夹</button></div>
          <SelectControl v-if="library.folders.length"><select id="favorite-save-folder" ref="folderSelect" v-model="folderId" :disabled="!!pending || !desktop" required><option v-for="folder in library.folders" :key="folder.id" :value="folder.id">{{ folder.name }}</option></select></SelectControl>
          <div v-if="newFolderOpen" id="favorite-new-folder" class="favorite-new-folder"><label class="field"><span>新收藏夹名称</span><input ref="folderNameInput" v-model="newFolderName" :disabled="!!pending || !desktop" maxlength="40" placeholder="例如：联通流量" @keydown.enter.prevent="createFolder" /></label><button class="button" type="button" :disabled="!!pending || !desktop || invalidFolderName" @click="createFolder"><LoaderCircle v-if="pending === 'folder'" :size="13" class="spinning" aria-hidden="true" />{{ pending === 'folder' ? '新建中…' : '新建' }}</button><p v-if="newFolderName.trim() && invalidFolderName" class="favorite-save-error">名称已存在，或超过 40 个字。</p></div>
          <p v-if="folderStatus" class="favorite-save-status" role="status"><Check :size="13" aria-hidden="true" /><span>{{ folderStatus }}</span></p>
          <p v-if="existing" class="favorite-existing-note"><CircleAlert :size="14" aria-hidden="true" /><span>已收藏此 IP。「更新本次测试」将替换原有测试快照和备注。</span></p>
          <label class="field favorite-save-note"><span class="favorite-note-label"><span>备注 <small>可选</small></span><span class="favorite-note-count">{{ note.length }} / 500</span></span><textarea v-model="note" :disabled="!!pending || !desktop" maxlength="500" placeholder="记录适用网络或使用体验" /></label>
        </div>
        <p v-if="error" class="favorite-save-error favorite-save-command-error" role="alert" tabindex="0" aria-label="提交错误，可滚动查看完整内容"><CircleAlert :size="14" aria-hidden="true" /><span>{{ error }}</span></p>
        <div class="favorite-save-actions"><span class="favorite-save-local">仅保存在本机</span><div><button class="button" type="button" :disabled="!!pending" @click="close">取消</button><button class="button primary" type="submit" :disabled="saveDisabled"><LoaderCircle v-if="pending === 'save'" :size="14" class="spinning" aria-hidden="true" /><Star v-else :size="14" aria-hidden="true" />{{ pending === 'save' ? '保存中…' : existing ? '更新本次测试' : '收藏本次测试' }}</button></div></div>
      </form>
    </dialog>
  </Teleport>
</template>

<style scoped>
.favorite-save-dialog { width: min(480px,calc(100vw - 32px)); max-height: calc(100dvh - 40px); padding: 0; overflow: hidden; border: 1px solid var(--line); border-radius: 12px; color: var(--text); background: var(--surface-raised); box-shadow: 0 20px 70px #23243e26, 0 4px 16px #23243e0c; }
.favorite-save-dialog::backdrop { background: #23243e42; backdrop-filter: blur(3px); }
.favorite-save-dialog>form { display: flex; flex-direction: column; max-height: calc(100dvh - 42px); min-height: 0; margin: 0; }
.favorite-save-heading { display: flex; align-items: center; justify-content: space-between; flex: 0 0 auto; gap: 12px; padding: 13px 18px; border-bottom: 1px solid var(--line); background: linear-gradient(180deg,var(--surface-raised),var(--surface-sunken)); }
.favorite-heading-title { display: inline-flex; align-items: center; gap: 10px; }
.favorite-heading-title>h2 { font-size: var(--dialog-title-size,14px); font-weight: 600; letter-spacing: -.15px; }
.favorite-heading-icon { display: inline-grid; place-items: center; width: var(--dialog-icon-size,28px); height: var(--dialog-icon-size,28px); border: 1px solid var(--selected-border); border-radius: 8px; color: var(--brand); background: var(--soft); box-shadow: inset 0 1px 0 color-mix(in srgb,var(--surface-raised) 65%,transparent); }
.favorite-save-heading>.icon-button { width: var(--dialog-icon-size,28px); height: var(--dialog-icon-size,28px); min-height: var(--dialog-icon-size,28px); color: var(--muted); border-radius: 6px; }
.favorite-save-heading>.icon-button:hover:not(:disabled) { color: var(--text); background: var(--line-subtle); }
.favorite-save-body { flex: 1 1 auto; min-height: 0; padding: 16px 18px; overflow-y: auto; overscroll-behavior: contain; }
.favorite-save-preview { padding: 11px 12px 0; border: 1px solid var(--line); border-radius: 8px; background: var(--surface-sunken); box-shadow: inset 0 1px 0 var(--surface-raised); }
.favorite-preview-identity { display: flex; align-items: center; justify-content: space-between; flex-wrap: wrap; gap: 6px 12px; }
.favorite-preview-identity>strong { min-width: 0; font-size: 16px; line-height: 22px; letter-spacing: -.25px; overflow-wrap: anywhere; }
.favorite-preview-location { display: inline-flex; align-items: center; gap: 6px; color: var(--muted); font-size: 11px; white-space: nowrap; }
.favorite-preview-pop { padding: 0 4px; border: 1px solid var(--line); border-radius: 4px; background: var(--surface-raised); color: var(--muted); font-size: 9px; line-height: 16px; letter-spacing: .2px; }
.favorite-preview-status { margin-top: 3px; color: var(--muted); font-size: 11px; line-height: 16px; }
.favorite-save-metrics { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 12px; margin-top: 9px; padding-bottom: 10px; }
.favorite-save-metrics>span { display: flex; align-items: baseline; gap: 8px; color: var(--muted); font-size: 11px; }
.favorite-save-metrics>span+span { border-left: 1px solid var(--line); padding-left: 14px; }
.favorite-save-metrics strong { color: var(--brand); font-size: 13px; font-weight: 600; font-variant-numeric: tabular-nums; white-space: nowrap; }
.favorite-save-metrics small { margin-left: 1px; color: var(--muted); font-size: 10px; font-weight: 400; }
.favorite-save-meta { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-inline: -12px; padding: 7px 12px; border-top: 1px solid var(--line); color: var(--muted); font-size: 10px; line-height: 14px; }
.favorite-save-meta>span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.favorite-save-meta>time { flex: 0 0 auto; font-variant-numeric: tabular-nums; }
.favorite-save-explanation { margin: 10px 0 12px; color: var(--muted); font-size: 11px; line-height: 18px; }
.favorite-save-folder-label { display: flex; align-items: center; justify-content: space-between; gap: 8px; min-height: 25px; margin-bottom: 5px; }
.favorite-save-folder-label>label { color: var(--text); font-size: 12px; font-weight: 500; }
.favorite-save-folder-label>.text-button { gap: 4px; min-height: 24px; padding: 0; font-size: 11px; }
.favorite-save-body :deep(select), .favorite-save-body input:not([type=checkbox]) { height: var(--dialog-control-height,32px); min-height: var(--dialog-control-height,32px); font-size: 12px; border-radius: 6px; background: var(--surface-raised); box-shadow: var(--control-shadow); }
.favorite-save-body :deep(select:hover:not(:disabled)), .favorite-save-body input:hover:not(:disabled), .favorite-save-body textarea:hover:not(:disabled) { border-color: var(--line-hover); }
.favorite-save-body :deep(select:focus-visible), .favorite-save-body input:focus-visible, .favorite-save-body textarea:focus-visible { outline: 2px solid color-mix(in srgb,var(--brand) 55%,transparent); outline-offset: 2px; border-color: var(--brand); }
.favorite-save-body :deep(select:disabled), .favorite-save-body input:not([type=checkbox]):disabled, .favorite-save-body textarea:disabled { color: var(--muted); background: var(--surface-sunken); box-shadow: none; cursor: default; }
.favorite-new-folder { display: flex; align-items: flex-end; gap: 8px; flex-wrap: wrap; margin-top: 8px; padding: 10px; border: 1px solid var(--selected-border); border-radius: 7px; background: color-mix(in srgb,var(--soft) 45%,var(--surface-raised)); }
.favorite-new-folder>.field { flex: 1 1 180px; gap: 5px; }
.favorite-new-folder>.field>span { font-size: 11px; line-height: 16px; }
.favorite-new-folder>.button { min-width: 58px; }
.favorite-new-folder>.favorite-save-error { flex-basis: 100%; margin-top: 0; }
.favorite-existing-note { display: flex; align-items: flex-start; gap: 7px; padding: 9px 10px; margin-top: 10px; border: 1px solid color-mix(in srgb,var(--warn) 16%,transparent); border-radius: 6px; background: var(--warn-bg); color: var(--warn); font-size: 11px; line-height: 18px; }
.favorite-existing-note>svg { margin-top: 2px; }
.favorite-save-note { margin-top: 12px; gap: 6px; }
.favorite-note-label { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; }
.favorite-note-label>span:first-child { color: var(--text); font-size: 12px; font-weight: 500; }
.favorite-note-label small { margin-left: 3px; color: var(--muted); font-size: 10px; font-weight: 400; }
.favorite-note-count { color: var(--muted); font-size: 10px; font-weight: 400; font-variant-numeric: tabular-nums; }
.favorite-save-note textarea { min-height: 64px; height: 64px; max-height: 160px; padding: 9px 10px; border-radius: 6px; font-family: inherit; font-size: 12px; line-height: 18px; background: var(--surface-raised); box-shadow: var(--control-shadow); }
.favorite-save-status { display: flex; align-items: flex-start; gap: 6px; margin-top: 8px; color: var(--brand); font-size: 11px; line-height: 18px; }
.favorite-save-status>svg { margin-top: 2px; }
.favorite-save-status>span { min-width: 0; overflow-wrap: anywhere; }
.favorite-save-error { color: var(--error); margin-top: 8px; font-size: 11px; line-height: 18px; overflow-wrap: anywhere; }
.favorite-save-command-error { display: flex; align-items: flex-start; flex: 0 0 auto; gap: 7px; max-height: 100px; margin: 0; padding: 8px 18px; overflow: auto; overscroll-behavior: contain; border: 0; border-top: 1px solid color-mix(in srgb,var(--error) 18%,var(--line)); border-radius: 0; color: var(--error); background: var(--error-bg); }
.favorite-save-command-error>svg { margin-top: 2px; }
.favorite-save-command-error>span { min-width: 0; overflow-wrap: anywhere; }
.favorite-save-actions { display: flex; align-items: center; justify-content: space-between; flex: 0 0 auto; gap: 12px; padding: 12px 18px; border-top: 1px solid var(--line); background: var(--surface-sunken); }
.favorite-save-local { color: var(--muted); font-size: 10px; white-space: nowrap; }
.favorite-save-actions>div { display: flex; align-items: center; gap: 8px; }
.favorite-save-dialog .button { min-height: var(--dialog-control-height,32px); font-size: 12px; border-radius: 6px; }
.favorite-save-actions .button:first-child { min-width: 64px; }
.favorite-save-actions .button.primary { min-width: 118px; }
@media (max-width: 400px) {
  .favorite-save-dialog { width: calc(100vw - 20px); }
  .favorite-save-heading, .favorite-save-body, .favorite-save-command-error, .favorite-save-actions { padding-inline: 14px; }
  .favorite-save-metrics { gap: 8px; }
  .favorite-save-metrics>span { gap: 5px; }
  .favorite-save-metrics>span+span { padding-left: 10px; }
  .favorite-save-local { display: none; }
  .favorite-save-actions { justify-content: flex-end; }
}
@media (max-height: 500px) { .favorite-save-dialog { max-height: calc(100dvh - 24px); } .favorite-save-dialog>form { max-height: calc(100dvh - 26px); } .favorite-save-heading { padding-block: 10px; } .favorite-save-body { padding-block: 12px; } .favorite-save-actions { padding-block: 10px; } }
</style>
