import { nextTick, onBeforeUnmount, onMounted, watch, type Ref } from 'vue'

type ElementRef = Readonly<Ref<HTMLElement | null>>
const openedEvent = 'ip-picker:popover-opened'

function announceOpen(owner: object) {
  document.dispatchEvent(new CustomEvent(openedEvent, { detail: { owner } }))
}

/** Close transient panels when navigation changes, without moving focus. */
export function dismissPopovers() { announceOpen({}) }

function containsEvent(element: HTMLElement | null, event: Event) {
  return !!element && (event.composedPath().includes(element)
    || event.target instanceof Node && element.contains(event.target))
}

function modalOpen() { return !!document.querySelector('dialog[open]') }

/** Dismiss only the selected floating details; inline disclosures stay open. */
export function useDismissibleDetails(root: ElementRef, selector: string) {
  let lastOpened: HTMLDetailsElement | null = null
  const details = () => [...root.value?.querySelectorAll<HTMLDetailsElement>(selector) ?? []]
  const onToggle = (event: Event) => {
    const target = event.target
    if (!(target instanceof HTMLDetailsElement) || !target.open
      || !root.value?.contains(target) || !target.matches(selector)) return
    lastOpened = target
    announceOpen(target)
  }
  const onOtherOpen = (event: Event) => {
    const owner = (event as CustomEvent<{ owner: object }>).detail?.owner
    for (const element of details()) if (element !== owner) element.open = false
  }
  const onPointerDown = (event: Event) => {
    for (const element of details()) {
      if (element.open && !containsEvent(element, event)) element.open = false
    }
  }
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape' || event.defaultPrevented || modalOpen()) return
    const opened = details().filter(element => element.open)
    const target = lastOpened?.open && opened.includes(lastOpened) ? lastOpened : opened.at(-1)
    if (!target) return
    event.preventDefault()
    for (const element of opened) element.open = false
    target.querySelector<HTMLElement>(':scope > summary')?.focus({ preventScroll: true })
  }
  onMounted(() => {
    document.addEventListener('toggle', onToggle, true)
    document.addEventListener(openedEvent, onOtherOpen)
    document.addEventListener('pointerdown', onPointerDown, true)
    document.addEventListener('keydown', onKeyDown)
    const opened = details().filter(element => element.open).at(-1)
    if (opened) { lastOpened = opened; announceOpen(opened) }
  })
  onBeforeUnmount(() => {
    document.removeEventListener('toggle', onToggle, true)
    document.removeEventListener(openedEvent, onOtherOpen)
    document.removeEventListener('pointerdown', onPointerDown, true)
    document.removeEventListener('keydown', onKeyDown)
  })
}

/** A controlled floating panel shares the same dismissal group as details. */
export function useDismissiblePopover(open: Ref<boolean>, panel: ElementRef, trigger: ElementRef) {
  const owner = {}
  let mounted = false
  const stopWatching = watch(open, value => {
    if (mounted && value) announceOpen(owner)
  }, { flush: 'post' })
  const onOtherOpen = (event: Event) => {
    if ((event as CustomEvent<{ owner: object }>).detail?.owner !== owner) open.value = false
  }
  const onPointerDown = (event: Event) => {
    if (open.value && !containsEvent(panel.value, event) && !containsEvent(trigger.value, event)) open.value = false
  }
  const onKeyDown = (event: KeyboardEvent) => {
    if (!open.value || event.key !== 'Escape' || event.defaultPrevented || modalOpen()) return
    event.preventDefault()
    open.value = false
    void nextTick(() => {
      if (trigger.value?.isConnected && !modalOpen()) trigger.value.focus({ preventScroll: true })
    })
  }
  onMounted(() => {
    mounted = true
    document.addEventListener(openedEvent, onOtherOpen)
    document.addEventListener('pointerdown', onPointerDown, true)
    document.addEventListener('keydown', onKeyDown)
    if (open.value) announceOpen(owner)
  })
  onBeforeUnmount(() => {
    mounted = false
    stopWatching()
    document.removeEventListener(openedEvent, onOtherOpen)
    document.removeEventListener('pointerdown', onPointerDown, true)
    document.removeEventListener('keydown', onKeyDown)
  })
}
