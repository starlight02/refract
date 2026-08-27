import { describe, expect, it } from 'vite-plus/test'
import { headerRowError, headersFromRows, rowsFromHeaders } from '../extra-headers'

describe('headerRowError', () => {
  it('ignores a blank row', () => {
    expect(headerRowError({ name: '', value: '' })).toBeNull()
  })

  it('rejects managed headers', () => {
    expect(headerRowError({ name: 'Authorization', value: 'Bearer x' })).toContain('掌管')
    expect(headerRowError({ name: 'x-api-key', value: 'k' })).toContain('掌管')
  })

  it('rejects CR/LF in the value', () => {
    expect(headerRowError({ name: 'x-route', value: 'a\nb' })).toContain('换行')
  })
})

describe('headersFromRows', () => {
  it('drops blank rows and keeps valid pairs', () => {
    expect(
      headersFromRows([
        { name: '', value: '' },
        { name: 'x-site-token', value: 'abc' },
      ]),
    ).toEqual({
      headers: [['x-site-token', 'abc']],
      error: null,
    })
  })

  it('round-trips saved headers', () => {
    const rows = rowsFromHeaders([['x-route', 'edge-1']])
    expect(headersFromRows(rows)).toEqual({
      headers: [['x-route', 'edge-1']],
      error: null,
    })
  })
})
