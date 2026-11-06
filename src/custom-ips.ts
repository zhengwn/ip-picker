export const MAX_CUSTOM_IP_BYTES = 2 * 1024 * 1024
const MAX_ENTRIES = 20_000
const IPV4_OCTET = /^(?:0|[1-9]\d{0,2})$/

function isIpv4(address: string, octetPattern: RegExp): boolean {
  const parts = address.split('.')
  return parts.length === 4 && parts.every(part => octetPattern.test(part) && Number(part) <= 255)
}

/** Match the native bare IPv4 parser. CIDR notation has its own parser. */
export function validManualIpv4(value: string): boolean {
  return isIpv4(value.trim(), IPV4_OCTET)
}

export function customIpsTooLarge(text: string): boolean {
  return text.length > MAX_CUSTOM_IP_BYTES || new TextEncoder().encode(text).length > MAX_CUSTOM_IP_BYTES
}

export function inspectCustomIps(text: string): { capacity: number; error?: never } | { capacity?: never; error: string } {
  if (customIpsTooLarge(text)) return { error: '自定义列表不能超过 2 MiB' }

  const unique = new Set<string>()
  let count = 0
  let capacity = 0
  for (const [index, line] of text.split('\n').entries()) {
    // Unicode White_Space matches Rust char::is_whitespace, including U+0085.
    for (const entry of line.split('#', 1)[0].split(/[\p{White_Space},;]+/u)) {
      if (!entry) continue
      if (++count > MAX_ENTRIES) return { error: '自定义列表最多接受 20000 个 IP / 网段条目' }
      const slash = entry.indexOf('/')
      let prefix = 32
      if (slash === -1) {
        if (!isIpv4(entry, IPV4_OCTET)) return { error: `第 ${index + 1} 行不是有效 IPv4：${entry}（仅支持 IPv4）` }
      } else {
        const prefixText = entry.slice(slash + 1)
        prefix = Number(prefixText)
        if (!/^\d{1,2}$/.test(prefixText) || prefix > 32 || !isIpv4(entry.slice(0, slash), /^\d{1,3}$/)) {
          return { error: `第 ${index + 1} 行不是有效 IPv4 CIDR：${entry}` }
        }
      }
      if (!unique.has(entry)) capacity += 2 ** (32 - prefix)
      unique.add(entry)
    }
  }
  return count ? { capacity } : { error: '请输入或导入 IPv4 / CIDR 列表' }
}
