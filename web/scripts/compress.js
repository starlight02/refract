import fs from 'node:fs'
import path from 'node:path'
import zlib from 'node:zlib'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const distDir = path.resolve(__dirname, '../dist')

if (!fs.existsSync(distDir)) {
  console.log(`[compress] ${distDir} does not exist, skipping.`)
  process.exit(0)
}

function walk(dir) {
  let results = []
  const list = fs.readdirSync(dir)
  for (const file of list) {
    const filePath = path.join(dir, file)
    const stat = fs.statSync(filePath)
    if (stat && stat.isDirectory()) {
      results = results.concat(walk(filePath))
    } else {
      results.push(filePath)
    }
  }
  return results
}

const compressibleExts = new Set([
  '.html',
  '.js',
  '.css',
  '.svg',
  '.json',
  '.ico',
  '.txt',
  '.xml',
  '.wasm',
])

const files = walk(distDir).filter((file) => {
  const ext = path.extname(file)
  return compressibleExts.has(ext) && !file.endsWith('.gz') && !file.endsWith('.br')
})

let totalRaw = 0
let totalGz = 0
let totalBr = 0

for (const file of files) {
  const content = fs.readFileSync(file)
  totalRaw += content.length

  // Gzip
  const gz = zlib.gzipSync(content, { level: 9 })
  fs.writeFileSync(`${file}.gz`, gz)
  totalGz += gz.length

  // Brotli
  const br = zlib.brotliCompressSync(content, {
    params: {
      [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
    },
  })
  fs.writeFileSync(`${file}.br`, br)
  totalBr += br.length
}

const savingsGz = totalRaw > 0 ? ((1 - totalGz / totalRaw) * 100).toFixed(1) : 0
const savingsBr = totalRaw > 0 ? ((1 - totalBr / totalRaw) * 100).toFixed(1) : 0

console.log(
  `[compress] Pre-compressed ${files.length} static assets:` +
    ` Raw=${(totalRaw / 1024).toFixed(1)}KB ->` +
    ` Gzip=${(totalGz / 1024).toFixed(1)}KB (↓${savingsGz}%) ->` +
    ` Brotli=${(totalBr / 1024).toFixed(1)}KB (↓${savingsBr}%)`,
)
