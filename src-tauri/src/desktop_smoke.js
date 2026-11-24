const note = '桌面冒烟收藏备注'
const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args)
const tcpMetricSelect = 'select[aria-label="TCP 延迟列显示指标"]'

async function waitFor(description, read) {
  const deadline = Date.now() + 8000
  while (Date.now() < deadline) {
    const result = read()
    if (result) return result
    await new Promise(resolve => setTimeout(resolve, 25))
  }
  throw new Error(`Timed out waiting for ${description}`)
}

function element(selector) {
  return waitFor(selector, () => document.querySelector(selector))
}

function button(selector, text) {
  return waitFor(text, () => {
    const node = document.querySelector(selector)
    return node?.textContent.trim() === text && !node.disabled ? node : null
  })
}

async function openHistory() {
  const history = await element('button[aria-controls="scan-history"]')
  history.click()
  await waitFor('saved wired retest in history', () =>
    document.querySelector('[aria-label="最近测速记录"]')?.textContent.includes('有线 · en8'))
}

async function exercise() {
  await button('.start-button', '开始完整优选')
  const budgetShortcut = await button('.scan-budget-toggle', '开启流量预算')
  budgetShortcut.click()
  await button('.scan-budget-toggle[aria-pressed="true"]', '预算已开启 · 400 MiB')
  budgetShortcut.click()
  await button('.scan-budget-toggle[aria-pressed="false"]', '开启流量预算')
  const { config } = await invoke('load_state')
  const invalid = { ...config, customIps: '127.0.0.1\nbad-ip' }
  for (const command of ['save_config', 'start_scan']) {
    const issue = await invoke(command, { config: invalid }).then(() => null, error => error)
    if (issue?.field !== 'customIps' || !issue.message?.includes('第 2 行')) {
      throw new Error(`Unexpected field error from ${command}: ${JSON.stringify(issue)}`)
    }
  }
  ;(await element('.advanced-trigger')).click()
  ;(await element('#settings-tab-quality')).click()
  await waitFor('budget controls moved into advanced settings', () => {
    const budget = document.querySelector('#setting-downloadBudgetEnabled')
    return budget?.closest('#settings-panel-quality') && budget.offsetParent !== null && !budget.checked
  })
  ;(await element('button[aria-label="关闭高级设置"]')).click()
  const quick = await button('.quick-scan-button', '快速验证')
  quick.click()
  await button('.start-button', '停止测速')
  await waitFor('quick run context and incremental stability progress', () => {
    const progress = document.querySelector('.scan-progress-disclosure summary')?.textContent
    return document.querySelector('.run-mode-label')?.textContent.includes('快速验证') &&
      progress?.includes('阶段 3 / 5') && progress.includes('已探测 2 / 100 次')
  })
  const progressSummary = await element('.scan-progress-disclosure summary')
  if (document.querySelector('.scan-progress-popover')) throw new Error('Closed progress details are still mounted')
  progressSummary.click()
  await element('.scan-progress-popover')
  progressSummary.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  await waitFor('progress details unmount on close', () => !document.querySelector('.scan-progress-popover'))
  const completedOnly = await element('input[aria-describedby="completed-download-help"]')
  if (completedOnly.checked) completedOnly.click()
  await element('.all-results-table .favorite-button')
  const tableBeforeDetails = document.querySelector('.all-results-table').getBoundingClientRect()
  if (tableBeforeDetails.top > 230 || tableBeforeDetails.height < window.innerHeight * .55) {
    throw new Error('Auxiliary controls occupy too much of the results viewport')
  }
  ;(await element('.run-summary-button')).click()
  await waitFor('actual run context available on demand', () => document.querySelector('.run-summary-panel')?.textContent.includes('10 次'))
  if (document.querySelector('.all-results-table').getBoundingClientRect().top !== tableBeforeDetails.top) {
    throw new Error('Opening run details moved the result table')
  }
  document.querySelector('.run-summary-button').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  await waitFor('run details close and restore focus', () => !document.querySelector('.run-summary-panel') && document.activeElement === document.querySelector('.run-summary-button'))
  const metric = await element(`.all-results-table ${tcpMetricSelect}`)
  metric.value = 'peak'
  metric.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor('peak column also sorts by peak', () => document.querySelector('select[aria-label="排序"]')?.value === 'peakTcp')
  const sort = await element('select[aria-label="排序"]')
  sort.value = 'averageTcp'
  sort.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor('average sorting updates the displayed column', () => metric.value === 'average')

  // Organizing saved addresses must not interrupt the running measurement.
  const scanBeforeOrganizing = await invoke('get_scan')
  ;(await element('button[aria-controls="favorites-library"]')).click()
  ;(await button('.favorite-top-actions .workspace-secondary-action', '新建收藏夹')).click()
  const folderName = await element('.favorite-modal[open] input')
  folderName.value = '冒烟分类'
  folderName.dispatchEvent(new Event('input', { bubbles: true }))
  ;(await button('.favorite-modal[open] button[type="submit"]', '新建收藏夹')).click()
  await waitFor('new folder while scanning', () => document.querySelector('.favorite-folder-list')?.textContent.includes('冒烟分类'))
  ;(await element('input[aria-label="仅看复测变化"]')).click()
  const search = await element('.favorite-search input')
  search.value = 'no-matching-address'
  search.dispatchEvent(new Event('input', { bubbles: true }))
  ;(await button('.favorite-top-actions .workspace-primary-action', '手动添加 IP')).click()
  const manualIp = await element('.favorite-modal[open] input[placeholder="例如：104.16.0.1"]')
  manualIp.value = '127.0.0.2'
  manualIp.dispatchEvent(new Event('input', { bubbles: true }))
  await waitFor('save-only enabled while retest is disabled', () => {
    const actions = [...document.querySelectorAll('.favorite-modal[open] .favorite-modal-actions button')]
    return actions.some(node => node.textContent.trim() === '收藏并测速' && node.disabled)
  })
  ;(await button('.favorite-modal[open] button[type="submit"]', '仅收藏')).click()
  await waitFor('untested saved row', () => document.querySelector('.favorites-table')?.textContent.includes('尚未测速'))
  if (search.value) throw new Error('The saved address is still hidden behind the previous search')
  const favoritesTable = document.querySelector('.favorites-table')
  const favoriteBounds = favoritesTable.getBoundingClientRect()
  if (favoriteBounds.top > 180 || favoriteBounds.height < window.innerHeight * .6) {
    throw new Error('Favorite metadata occupies too much of the list viewport')
  }
  ;(await element('.favorite-more-trigger')).click()
  await waitFor('favorite details available without moving the list', () => document.querySelector('#favorite-more-panel')?.textContent.includes('下次复测参数'))
  if (favoritesTable.getBoundingClientRect().top !== favoriteBounds.top) throw new Error('Favorite details pushed the list down')
  document.querySelector('.favorite-more-trigger').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  await waitFor('favorite details close and restore focus', () => !document.querySelector('#favorite-more-panel') && document.activeElement === document.querySelector('.favorite-more-trigger'))
  ;(await element('.favorite-more-trigger')).click()
  ;(await waitFor('rename folder in management details', () => [...document.querySelectorAll('.favorite-manage-actions button')].find(node => node.textContent.trim() === '重命名收藏夹'))).click()
  ;(await button('.favorite-modal[open] button[type="button"]:not(.icon-button)', '取消')).click()
  await waitFor('return from folder editor to details trigger', () => !document.querySelector('.favorite-modal[open]') && document.activeElement === document.querySelector('.favorite-more-trigger'))

  const savedToggle = await element('.favorite-row-toggle')
  savedToggle.click()
  await waitFor('save-only details never claim a test start', () =>
    [...document.querySelectorAll('.favorite-detail-grid>div')].some(node => node.querySelector('dt')?.textContent === '测试开始' && node.querySelector('dd')?.textContent === '尚未测速'))
  savedToggle.click()
  ;(await element('button[title="编辑分类与备注"]')).click()
  const editNote = await element('.favorite-modal[open] textarea')
  editNote.value = '等待期间整理的备注'
  editNote.dispatchEvent(new Event('input', { bubbles: true }))
  ;(await button('.favorite-modal[open] button[type="submit"]', '保存修改')).click()
  await waitFor('note editor closed', () => !document.querySelector('.favorite-modal[open]'))
  const organized = await invoke('get_favorites')
  const savedOnly = organized.entries.find(entry => entry.result.ip === '127.0.0.2')
  const scanAfterOrganizing = await invoke('get_scan')
  if (!savedOnly || savedOnly.runId || savedOnly.note !== '等待期间整理的备注' || scanAfterOrganizing.id !== scanBeforeOrganizing.id || scanAfterOrganizing.status !== 'running') {
    throw new Error('Save-only or note editing changed the active scan or lost its metadata')
  }

  await openHistory()
  const stop = await button('.header-stop', '停止测速')
  stop.click()
  await waitFor('history refreshed after stopping a scan', () =>
    document.querySelector('[aria-label="最近测速记录"]')?.textContent.includes('Wi‑Fi · en0'))
  const scan = await waitFor('regional scan navigation', () =>
    [...document.querySelectorAll('.workspace-nav button')].find(node => node.textContent.trim() === '地区优选'))
  scan.click()
  await waitFor('cancelled status', () =>
    document.querySelector('.all-results-heading .status-label')?.textContent.trim() === '已停止')

  const star = await element('.all-results-table .favorite-button')
  star.click()
  const noteInput = await element('.favorite-save-dialog[open] textarea')
  noteInput.value = note
  noteInput.dispatchEvent(new Event('input', { bubbles: true }))
  const save = await button('.favorite-save-dialog[open] button[type="submit"]', '收藏本次测试')
  save.click()
  await element('.all-results-table .favorite-button.is-saved')

  // A debounced preference must be flushed by the native close request.
  const japan = await waitFor('selected Japan region', () =>
    [...document.querySelectorAll('.region-choice[aria-pressed="true"]')]
      .find(node => node.textContent.includes('日本')))
  japan.click()

  await openHistory()
  const historyInput = await element('#history-ip-search')
  historyInput.value = '127.0.0.1'
  historyInput.dispatchEvent(new Event('input', { bubbles: true }))
  ;(await button('.history-search button', '对比 IP')).click()
  const wired = await waitFor('wired measurement to pin', () => [...document.querySelectorAll('.history-data-row')].find(row => row.textContent.includes('有线 · en8')))
  wired.querySelector('.history-result-toggle').click()
  ;(await button('.history-pin-button', '固定为基线')).click()
  await element('.history-baseline-bar')

  const { entries } = await invoke('get_favorites')
  const favorite = entries.find(entry => entry.result.ip === '127.0.0.1')
  const issue = await invoke('start_favorite_test', {
    favoriteId: favorite.id,
    downloadOnly: true,
    downloadSettings: {
      targetUrl: 'https://ip-picker-fixture.invalid/download',
      connectTimeoutMs: 1500,
      requestTimeoutMs: 5000,
      downloadRounds: 1,
      downloadSeconds: 3,
      downloadBudgetEnabled: false,
    },
  }).then(() => null, error => error)
  if (!String(issue).includes('下载复测须为 2 至 5 轮')) {
    throw new Error(`Download-only settings were not validated by the native command: ${issue}`)
  }
}

async function reopen() {
  await button('.start-button', '重新完整优选')
  await waitFor('saved cancelled run', () =>
    document.querySelector('.all-results-heading .status-label')?.textContent.trim() === '已停止')
  await waitFor('persisted quick run identity', () => document.querySelector('.run-mode-label')?.textContent.includes('快速验证'))
  await element('#scan-estimate-detail')
  await waitFor('setting saved when closing', () =>
    [...document.querySelectorAll('.region-choice[aria-pressed="false"]')]
      .some(node => node.textContent.includes('日本')))
  const favorites = await element('button[aria-controls="favorites-library"]')
  favorites.click()
  const details = await element('.favorite-row-toggle')
  if (details.textContent.trim() !== '127.0.0.1') throw new Error('The saved IP did not reopen')
  details.click()
  await waitFor('saved favorite note', () =>
    document.querySelector('.favorite-note-full')?.textContent.trim() === note)
  await element('#favorites-library tbody .tcp-probe-chart')
  const favoriteMetric = await element(`#favorites-library ${tcpMetricSelect}`)
  favoriteMetric.value = 'average'
  favoriteMetric.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor('average selection shared with regional results', () =>
    document.querySelector(`.all-results-table ${tcpMetricSelect}`)?.value === 'average')

  const compare = await element('button[title="对比该 IP 历史"]')
  compare.click()
  const comparison = await element('[aria-label="同一 IP 的各轮测速结果"]')
  if (comparison.querySelectorAll('tbody tr').length !== 2 ||
      !comparison.textContent.includes('有线 · en8') || !comparison.textContent.includes('Wi‑Fi · en0')) {
    throw new Error('The same-IP history did not restore both interface observations')
  }
  const comparisonBounds = comparison.getBoundingClientRect()
  if (comparisonBounds.top > 150 || comparisonBounds.height < window.innerHeight * .65 ||
      [...comparison.querySelectorAll('.history-data-row')].some(row => row.getBoundingClientRect().height > 56)) {
    throw new Error('History comparison is crowded by always-visible metadata')
  }
  ;(await element('.history-info-trigger')).click()
  await waitFor('comparison conditions available on demand', () => document.querySelector('#history-info-panel')?.textContent.includes('网络接口不同'))
  if (comparison.getBoundingClientRect().top !== comparisonBounds.top) throw new Error('History conditions moved the comparison table')
  document.querySelector('.history-info-trigger').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  await waitFor('history conditions close and restore focus', () => !document.querySelector('#history-info-panel') && document.activeElement === document.querySelector('.history-info-trigger'))
  const wifi = [...comparison.querySelectorAll('tbody tr')].find(row => row.textContent.includes('Wi‑Fi · en0'))
  if (comparison.querySelector(tcpMetricSelect)?.value !== 'average' ||
      !wifi.querySelector('.tcp-probe-chart') || comparison.querySelectorAll('.tcp-probe-chart').length !== 1) {
    throw new Error('The TCP comparison did not share the metric and chart only saved samples')
  }
  wifi.querySelector('.history-result-toggle').click()
  const historyDetails = await element('#history-tcp-detail')
  if (!historyDetails.textContent.includes('TCP 初筛') || !historyDetails.textContent.includes('采样 IP') || !historyDetails.textContent.includes('测速目标')) {
    throw new Error('Compact history rows lost their original parameters or network evidence')
  }
  const tcp = await element('#history-tcp-detail [aria-label="TCP 诊断明细"]')
  if (!tcp.textContent.includes('412.0 ms') || tcp.textContent.includes('测满后的 P95')) {
    throw new Error('The history did not display the partial TCP latency correctly')
  }
  tcp.querySelector('summary').click()
  const probes = await element('#history-tcp-detail [aria-label="逐次 TCP 探测结果"]')
  if (!probes.textContent.includes('连接超时') || !probes.textContent.includes('1501.2 ms')) {
    throw new Error('The history did not restore the failed probe and measured wait')
  }
  wifi.querySelector('.history-time').click()
  const detail = await element('[aria-label="本轮 IP 测速记录"]')
  if (detail.getBoundingClientRect().top > 180) throw new Error('Single-run history controls leave too little room for IP records')
  if (!detail.textContent.includes('127.0.0.1') || detail.querySelector(tcpMetricSelect)?.value !== 'average' ||
      !detail.querySelector('.tcp-probe-chart')) {
    throw new Error('The saved run did not reopen with the shared TCP column')
  }
  ;(await button('.history-back', '返回对比')).click()
  await element('.history-baseline-bar')
  ;(await button('.history-baseline-bar .button', '按基线参数复测')).click()
  await button('.start-button', '停止测速')
  const replay = await invoke('get_scan')
  if (replay.retestIp !== '127.0.0.1' || replay.config.stabilityAttempts !== 110 || replay.quickValidation) {
    throw new Error('The retest did not use the saved baseline or targeted IP')
  }
  ;(await button('.start-button', '停止测速')).click()
  await waitFor('retest stopped', () => document.querySelector('.all-results-heading .status-label')?.textContent.trim() === '已停止')
}

function measureDomChange(description, read, change) {
  return new Promise((resolve, reject) => {
    const started = performance.now()
    const observer = new MutationObserver(() => {
      if (!read()) return
      observer.disconnect()
      clearTimeout(timeout)
      resolve(performance.now() - started)
    })
    const timeout = setTimeout(() => {
      observer.disconnect()
      reject(new Error(`Timed out measuring ${description}`))
    }, 8000)
    observer.observe(document.body, { childList: true, subtree: true, characterData: true, attributes: true })
    change()
  })
}

async function exercisePerformance() {
  const candidates = Number(phase.split('-')[1])
  let snapshot
  let downloadUpdate
  let roundUpdate
  await invoke('plugin:event|listen', {
    event: 'scan-update',
    target: { kind: 'Any' },
    handler: window.__TAURI_INTERNALS__.transformCallback(({ payload }) => {
      if (payload.results?.length === candidates) snapshot ??= payload
      if (payload.changedRows?.[0]) roundUpdate ??= payload
      if (payload.downloadedBytes > 0 && !payload.results && !payload.changedRows) downloadUpdate ??= payload
    }),
  })
  ;(await button('.start-button', '开始完整优选')).click()
  await waitFor('full snapshot and compact download updates', () => snapshot && downloadUpdate && roundUpdate &&
    Number.parseFloat(document.querySelector('.scan-progress-disclosure summary .downloaded-total')?.textContent) > 0)
  const current = await invoke('get_scan')
  if (Object.keys(roundUpdate.changedRows).length !== 1 || roundUpdate.completed !== 1 ||
      current.progress.completed !== 1 || current.results[0].downloadSamplesMbps.at(-1) !== 20) {
    throw new Error('The compact download update lost a completed round or its row')
  }
  const jsonBytes = value => new TextEncoder().encode(JSON.stringify(value)).length
  ;(await element('.run-summary-button')).click()
  ;(await waitFor('initial issues action', () => [...document.querySelectorAll('.run-summary-panel button')].find(node => node.textContent.includes('查看初筛异常')))).click()
  const search = await element('input[aria-label="搜索初筛异常 IP"]')
  const timings = []
  for (let index = 0; index < 8; index++) {
    const ip = `198.18.3.20${index}`
    timings.push(await measureDomChange('filtered candidate row', () => {
      const results = document.querySelectorAll('.all-results-table .result-ip')
      return results.length === 1 && results[0].textContent === ip
    }, () => {
      search.value = ip
      search.dispatchEvent(new Event('input', { bubbles: true }))
    }))
  }
  const stop = await button('.start-button', '停止测速')
  const stopMs = await measureDomChange('terminal status',
    () => document.querySelector('.all-results-heading .status-label')?.textContent.trim() === '已停止',
    () => stop.click())
  await invoke('plugin:event|emit', {
    event: 'ip-picker-smoke-metrics',
    payload: {
      candidates,
      fullSnapshotBytes: jsonBytes(snapshot),
      downloadUpdateBytes: jsonBytes(downloadUpdate),
      roundUpdateBytes: jsonBytes(roundUpdate),
      medianFilterMs: timings.sort((a, b) => a - b).slice(3, 5).reduce((sum, value) => sum + value, 0) / 2,
      stopMs,
    },
  })
}

function report(error) {
  return invoke('plugin:event|emit', {
    event: 'ip-picker-smoke-finished',
    payload: error,
  })
}

const run = phase === 'exercise' ? exercise : phase === 'reopen' ? reopen : exercisePerformance
void run().then(() => report(null), error => report(String(error)))
