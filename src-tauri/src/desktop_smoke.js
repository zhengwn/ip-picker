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
  await button('.start-button', '开始测速')
  const { config } = await invoke('load_state')
  const invalid = { ...config, customIps: '127.0.0.1\nbad-ip' }
  for (const command of ['save_config', 'start_scan']) {
    const issue = await invoke(command, { config: invalid }).then(() => null, error => error)
    if (issue?.field !== 'customIps' || !issue.message?.includes('第 2 行')) {
      throw new Error(`Unexpected field error from ${command}: ${JSON.stringify(issue)}`)
    }
  }
  const quick = await button('.quick-scan-button', '快速验证 ≤ 64 MiB')
  quick.click()
  await button('.start-button', '停止测速')
  const completedOnly = await element('input[aria-describedby="completed-download-help"]')
  if (completedOnly.checked) completedOnly.click()
  await element('.all-results-table .favorite-button')

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
}

async function reopen() {
  await button('.start-button', '重新测速')
  await waitFor('saved cancelled run', () =>
    document.querySelector('.all-results-heading .status-label')?.textContent.trim() === '已停止')
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
  const wifi = [...comparison.querySelectorAll('tbody tr')].find(row => row.textContent.includes('Wi‑Fi · en0'))
  if (comparison.querySelector(tcpMetricSelect)?.value !== 'average' ||
      !wifi.querySelector('.tcp-probe-chart') || comparison.querySelectorAll('.tcp-probe-chart').length !== 1) {
    throw new Error('The TCP comparison did not share the metric and chart only saved samples')
  }
  wifi.querySelector('.history-result-toggle').click()
  const tcp = await element('#history-tcp-detail [aria-label="TCP 诊断明细"]')
  if (!tcp.textContent.includes('412.0 ms') || tcp.textContent.includes('测满后的 P95')) {
    throw new Error('The history did not display the partial TCP latency correctly')
  }
  tcp.querySelector('summary').click()
  const probes = await element('#history-tcp-detail [aria-label="逐次 TCP 探测结果"]')
  if (!probes.textContent.includes('连接超时') || !probes.textContent.includes('1501.2 ms')) {
    throw new Error('The history did not restore the failed probe and measured wait')
  }
  comparison.querySelector('tbody button').click()
  const detail = await element('[aria-label="本轮 IP 测速记录"]')
  if (!detail.textContent.includes('127.0.0.1') || detail.querySelector(tcpMetricSelect)?.value !== 'average' ||
      !detail.querySelector('.tcp-probe-chart')) {
    throw new Error('The saved run did not reopen with the shared TCP column')
  }
}

function report(error) {
  return invoke('plugin:event|emit', {
    event: 'ip-picker-smoke-finished',
    payload: error,
  })
}

const run = phase === 'exercise' ? exercise : reopen
void run().then(() => report(null), error => report(String(error)))
