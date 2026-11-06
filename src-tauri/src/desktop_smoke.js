const note = '桌面冒烟收藏备注'
const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args)

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

async function exercise() {
  const start = await button('.start-button', '开始测速')
  const { config } = await invoke('load_state')
  const invalid = { ...config, customIps: '127.0.0.1\nbad-ip' }
  for (const command of ['save_config', 'start_scan']) {
    const issue = await invoke(command, { config: invalid }).then(() => null, error => error)
    if (issue?.field !== 'customIps' || !issue.message?.includes('第 2 行')) {
      throw new Error(`Unexpected field error from ${command}: ${JSON.stringify(issue)}`)
    }
  }
  start.click()
  await button('.start-button', '停止测速')
  const completedOnly = await element('input[aria-describedby="completed-download-help"]')
  if (completedOnly.checked) completedOnly.click()
  await element('.all-results-table .favorite-button')

  const stop = await button('.start-button', '停止测速')
  stop.click()
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
}

function report(error) {
  return invoke('plugin:event|emit', {
    event: 'ip-picker-smoke-finished',
    payload: error,
  })
}

const run = phase === 'exercise' ? exercise : reopen
void run().then(() => report(null), error => report(String(error)))
