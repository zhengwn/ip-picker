export const MAX_CUSTOM_IP_BYTES = 2 * 1024 * 1024
const MAX_ENTRIES = 20_000
const IPV4_OCTET = /^(?:0|[1-9]\d{0,2})$/

function ipv4Number(address: string, octetPattern: RegExp): number | null {
  const parts = address.split('.')
  if (parts.length !== 4 || !parts.every(part => octetPattern.test(part) && Number(part) <= 255)) return null
  return parts.reduce((value, part) => value * 256 + Number(part), 0)
}

/** Match the native bare IPv4 parser. CIDR notation has its own parser. */
export function validManualIpv4(value: string): boolean {
  return ipv4Number(value.trim(), IPV4_OCTET) !== null
}

export function customIpsTooLarge(text: string): boolean {
  return text.length > MAX_CUSTOM_IP_BYTES || new TextEncoder().encode(text).length > MAX_CUSTOM_IP_BYTES
}

export function inspectCustomIps(text: string): { capacity: number; error?: never } | { capacity?: never; error: string } {
  if (customIpsTooLarge(text)) return { error: '自定义列表不能超过 2 MiB' }

  const ranges: [start: number, end: number][] = []
  for (const [index, line] of text.split('\n').entries()) {
    // Unicode White_Space matches Rust char::is_whitespace, including U+0085.
    for (const entry of line.split('#', 1)[0].split(/[\p{White_Space},;]+/u)) {
      if (!entry) continue
      if (ranges.length === MAX_ENTRIES) return { error: '自定义列表最多接受 20000 个 IP / 网段条目' }
      const slash = entry.indexOf('/')
      const address = slash === -1 ? ipv4Number(entry, IPV4_OCTET) : ipv4Number(entry.slice(0, slash), /^\d{1,3}$/)
      if (slash === -1 && address === null) return { error: `第 ${index + 1} 行不是有效 IPv4：${entry}（仅支持 IPv4）` }
      const prefixText = slash === -1 ? '32' : entry.slice(slash + 1)
      const prefix = Number(prefixText)
      if (address === null || !/^\d{1,2}$/.test(prefixText) || prefix > 32) {
        return { error: `第 ${index + 1} 行不是有效 IPv4 CIDR：${entry}` }
      }
      const size = 2 ** (32 - prefix)
      const start = Math.floor(address / size) * size
      ranges.push([start, start + size])
    }
  }
  if (!ranges.length) return { error: '请输入或导入 IPv4 / CIDR 列表' }

  ranges.sort(([a], [b]) => a - b)
  let capacity = 0
  let coveredEnd = 0
  for (const [start, end] of ranges) {
    if (end <= coveredEnd) continue
    capacity += end - Math.max(start, coveredEnd)
    coveredEnd = end
  }
  return { capacity }
}
