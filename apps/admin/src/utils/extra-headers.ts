/**
 * 渠道自定义请求头的行编辑草稿。
 *
 * 校验与后端 `Channel::validate` 对齐：头名必须是可见 ASCII，
 * 鉴权/传输语义头由网关掌管，值里禁止 CR/LF。
 */
export interface HeaderRow {
  name: string
  value: string
}

const FORBIDDEN_HEADERS: Record<string, true> = {
  authorization: true,
  host: true,
  'content-length': true,
  'content-type': true,
  'x-api-key': true,
}

export function emptyHeaderRow(): HeaderRow {
  return { name: '', value: '' }
}

export function rowsFromHeaders(headers: [string, string][] | undefined): HeaderRow[] {
  const rows = (headers ?? []).map(([name, value]) => ({ name, value }))
  return rows.length > 0 ? rows : [emptyHeaderRow()]
}

export function headerRowError(row: HeaderRow): string | null {
  const name = row.name.trim()
  if (!name && !row.value.trim()) return null
  if (!name) return '头名不能为空'
  const normalized = name.toLowerCase()
  for (let i = 0; i < normalized.length; i += 1) {
    const code = normalized.charCodeAt(i)
    if (code < 0x21 || code > 0x7e) {
      return `头名 \`${name}\` 不是合法的 HTTP header 名`
    }
  }
  if (FORBIDDEN_HEADERS[normalized]) {
    return `头 \`${normalized}\` 由网关掌管，不能覆盖`
  }
  if (row.value.includes('\r') || row.value.includes('\n')) {
    return `头 \`${name}\` 的值不能包含换行`
  }
  return null
}

export function headersFromRows(rows: HeaderRow[]): {
  headers: [string, string][]
  error: string | null
} {
  const headers: [string, string][] = []
  for (const row of rows) {
    const error = headerRowError(row)
    if (error) return { headers, error }
    const name = row.name.trim()
    if (!name) continue
    headers.push([name, row.value])
  }
  return { headers, error: null }
}

export function headerRowsError(rows: HeaderRow[]): string | null {
  return headersFromRows(rows).error
}
