/**
 * Liquid Glass SVG Filter Engine
 * 基于物理斯涅尔定律（Snell's Law）折射模型与高光贴图生成器。
 * 算法原型参考 archisvaze/liquid-glass 物理光学实现。
 */

export interface LiquidGlassOptions {
  glassThickness?: number
  bezelWidth?: number
  refractiveIndex?: number
  scaleRatio?: number
  blurAmount?: number
  specularOpacity?: number
  specularSaturation?: number
}

const SURFACE_FNS = {
  spherical: (x: number) => Math.sqrt(Math.max(0, 1 - x * x)),
  toroidal: (x: number) => Math.sqrt(Math.max(0, 0.25 - (x - 0.5) ** 2)),
  parabolic: (x: number) => 1 - x * x,
  flat: () => 1,
  convex_squircle: (x: number) => Math.pow(Math.max(0, 1 - Math.pow(x, 4)), 0.25),
  linear: (x: number) => 1 - x,
}

function calculateRefractionProfile(
  glassThickness: number,
  bezelWidth: number,
  heightFn: (x: number) => number,
  ior: number,
  samples = 128,
): Float64Array {
  const profile = new Float64Array(samples)
  const n1 = 1.0 // 空气折射率
  const n2 = ior // 玻璃折射率

  for (let i = 0; i < samples; i++) {
    const x = i / (samples - 1)
    const h = heightFn(x) * glassThickness
    const dx = 0.001
    const x1 = Math.max(0, x - dx)
    const x2 = Math.min(1, x + dx)
    const dh = ((heightFn(x2) - heightFn(x1)) * glassThickness) / (x2 - x1)
    const alpha = Math.atan(dh)
    const theta1 = alpha
    const sinTheta2 = (n1 / n2) * Math.sin(theta1)

    if (Math.abs(sinTheta2) <= 1) {
      const theta2 = Math.asin(sinTheta2)
      const delta = theta1 - theta2
      const disp = h * Math.tan(delta)
      profile[i] = disp
    } else {
      profile[i] = 0
    }
  }
  return profile
}

function generateDisplacementMap(
  w: number,
  h: number,
  radius: number,
  bezelWidth: number,
  profile: Float64Array,
  maxDisp: number,
): string {
  const c = document.createElement('canvas')
  c.width = w
  c.height = h
  const ctx = c.getContext('2d')
  if (!ctx) return ''

  const img = ctx.createImageData(w, h)
  const d = img.data
  const r = radius
  const rSq = r * r
  const rBSq = Math.max(r - bezelWidth, 0) ** 2
  const wB = w - r * 2
  const hB = h - r * 2

  for (let y1 = 0; y1 < h; y1++) {
    for (let x1 = 0; x1 < w; x1++) {
      const x = x1 < r ? x1 - r : x1 >= w - r ? x1 - r - wB : 0
      const y = y1 < r ? y1 - r : y1 >= h - r ? y1 - r - hB : 0
      const dSq = x * x + y * y
      const idx = (y1 * w + x1) * 4

      if (dSq > rSq || dSq < rBSq) {
        d[idx] = 128
        d[idx + 1] = 128
        d[idx + 2] = 128
        d[idx + 3] = 255
        continue
      }

      const dist = Math.sqrt(dSq)
      const fromEdge = r - dist
      const normDist = Math.max(0, Math.min(1, fromEdge / bezelWidth))
      const sampleIdx = Math.min(
        profile.length - 1,
        Math.floor((1 - normDist) * (profile.length - 1)),
      )
      const disp = profile[sampleIdx] ?? 0
      const cos = x / (dist || 1)
      const sin = y / (dist || 1)
      const dxNorm = ((-disp * cos) / maxDisp) * 0.5 + 0.5
      const dyNorm = ((-disp * sin) / maxDisp) * 0.5 + 0.5

      d[idx] = Math.round(Math.max(0, Math.min(255, dxNorm * 255)))
      d[idx + 1] = Math.round(Math.max(0, Math.min(255, dyNorm * 255)))
      d[idx + 2] = 128
      d[idx + 3] = 255
    }
  }
  ctx.putImageData(img, 0, 0)
  return c.toDataURL()
}

function generateSpecularMap(
  w: number,
  h: number,
  radius: number,
  bezelWidth: number,
  angle = Math.PI / 3,
): string {
  const c = document.createElement('canvas')
  c.width = w
  c.height = h
  const ctx = c.getContext('2d')
  if (!ctx) return ''

  const img = ctx.createImageData(w, h)
  const d = img.data
  const r = radius
  const rSq = r * r
  const r1Sq = (r + 1) ** 2
  const rBSq = Math.max(r - bezelWidth, 0) ** 2
  const wB = w - r * 2
  const hB = h - r * 2
  const svX = Math.cos(angle)
  const svY = Math.sin(angle)

  for (let y1 = 0; y1 < h; y1++) {
    for (let x1 = 0; x1 < w; x1++) {
      const x = x1 < r ? x1 - r : x1 >= w - r ? x1 - r - wB : 0
      const y = y1 < r ? y1 - r : y1 >= h - r ? y1 - r - hB : 0
      const dSq = x * x + y * y
      if (dSq > r1Sq || dSq < rBSq) continue
      const dist = Math.sqrt(dSq)
      const fromSide = r - dist
      const op = dSq < rSq ? 1 : 1 - (dist - Math.sqrt(rSq)) / (Math.sqrt(r1Sq) - Math.sqrt(rSq))
      if (op <= 0 || dist === 0) continue
      const cos = x / dist
      const sin = -y / dist
      const dot = Math.abs(cos * svX + sin * svY)
      const edge = Math.sqrt(Math.max(0, 1 - (1 - fromSide) ** 2))
      const coeff = dot * edge
      const col = (255 * coeff) | 0
      const alpha = (col * coeff * op) | 0
      const idx = (y1 * w + x1) * 4
      d[idx] = col
      d[idx + 1] = col
      d[idx + 2] = col
      d[idx + 3] = alpha
    }
  }
  ctx.putImageData(img, 0, 0)
  return c.toDataURL()
}

export function initLiquidGlass(
  containerId = 'liquid-glass-svg-defs',
  opts: LiquidGlassOptions = {},
) {
  if (typeof window === 'undefined') return

  const w = 400
  const h = 300
  const radius = 28
  const bezelWidth = opts.bezelWidth ?? 36
  const glassThickness = opts.glassThickness ?? 45
  const ior = opts.refractiveIndex ?? 2.4
  const scaleRatio = opts.scaleRatio ?? 0.8
  const blurAmt = opts.blurAmount ?? 0.3
  const specOpacity = opts.specularOpacity ?? 0.5
  const specSat = opts.specularSaturation ?? 3.5

  const heightFn = SURFACE_FNS.convex_squircle
  const clampedBezel = Math.min(bezelWidth, radius - 1, Math.min(w, h) / 2 - 1)

  const profile = calculateRefractionProfile(glassThickness, clampedBezel, heightFn, ior, 128)
  const maxDisp = Math.max(...Array.from(profile).map(Math.abs)) || 1
  const dispUrl = generateDisplacementMap(w, h, radius, clampedBezel, profile, maxDisp)
  const specUrl = generateSpecularMap(w, h, radius, clampedBezel * 1.8)
  const scale = maxDisp * scaleRatio

  let defs: Element | null = document.getElementById(containerId)
  if (!defs) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg')
    svg.setAttribute('width', '0')
    svg.setAttribute('height', '0')
    svg.style.position = 'absolute'
    svg.style.overflow = 'hidden'
    svg.style.pointerEvents = 'none'
    svg.setAttribute('color-interpolation-filters', 'sRGB')
    const newDefs = document.createElementNS('http://www.w3.org/2000/svg', 'defs')
    newDefs.id = containerId
    svg.appendChild(newDefs)
    document.body.appendChild(svg)
    defs = newDefs
  }

  defs.innerHTML = `
    <filter id="liquid-glass-filter" x="0%" y="0%" width="100%" height="100%" color-interpolation-filters="sRGB">
      <feGaussianBlur in="SourceGraphic" stdDeviation="${blurAmt}" result="blurred_source" />
      <feImage href="${dispUrl}" x="0" y="0" width="100%" height="100%" preserveAspectRatio="none" result="disp_map" />
      <feDisplacementMap in="blurred_source" in2="disp_map"
        scale="${scale}" xChannelSelector="R" yChannelSelector="G"
        result="displaced" />
      <feColorMatrix in="displaced" type="saturate" values="${specSat}" result="displaced_sat" />
      <feImage href="${specUrl}" x="0" y="0" width="100%" height="100%" preserveAspectRatio="none" result="spec_layer" />
      <feComposite in="displaced_sat" in2="spec_layer" operator="in" result="spec_masked" />
      <feComponentTransfer in="spec_layer" result="spec_faded">
        <feFuncA type="linear" slope="${specOpacity}" />
      </feComponentTransfer>
      <feBlend in="spec_masked" in2="displaced" mode="normal" result="with_sat" />
      <feBlend in="spec_faded" in2="with_sat" mode="normal" />
    </filter>
  `
}
