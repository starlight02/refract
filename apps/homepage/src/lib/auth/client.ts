import { GROK_PROVIDERS } from './providers'

export const authEnabled = false

export async function signIn(providerId?: string, options?: { callbackURL?: string }) {
  if (options?.callbackURL) {
    window.location.href = options.callbackURL
  }
}

export async function signOut() {
  window.location.reload()
}

export { GROK_PROVIDERS }
