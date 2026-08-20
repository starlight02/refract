export function Atmosphere() {
  return (
    <div aria-hidden="true" className="pointer-events-none fixed inset-0 -z-10 overflow-hidden">
      <div className="absolute inset-0 bg-bg" />
      <div className="optical-grid absolute inset-0" />
      <div className="vignette absolute inset-0" />
    </div>
  )
}
