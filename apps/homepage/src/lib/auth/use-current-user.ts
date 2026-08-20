export type AppUser = {
  id: string
  displayName: string | null
  primaryEmail: string | null
  profileImageUrl: string | null
  isDevFallback: boolean
}

export const DEV_USER: AppUser = {
  id: 'dev-user',
  displayName: 'Dev User',
  primaryEmail: 'dev@example.com',
  profileImageUrl: null,
  isDevFallback: true,
}

export type CurrentUserState = {
  user: AppUser | null
  isPending: boolean
}

export function useCurrentUserState(): CurrentUserState {
  return { user: null, isPending: false }
}

export function useCurrentUser(): AppUser | null {
  return null
}
