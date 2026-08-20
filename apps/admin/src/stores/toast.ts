import { ref } from 'vue'
import { defineStore } from 'pinia'

export type ToastTone = 'success' | 'danger' | 'warning' | 'info'

export interface ToastItem {
  id: string
  tone: ToastTone
  text: string
  description?: string
  duration?: number
  timer?: ReturnType<typeof setTimeout>
}

export interface ShowToastOptions {
  tone?: ToastTone
  text: string
  description?: string
  duration?: number
}

let nextId = 1

export const useToastStore = defineStore('toast', () => {
  const items = ref<ToastItem[]>([])

  function dismiss(id: string) {
    const index = items.value.findIndex((item) => item.id === id)
    if (index !== -1) {
      const item = items.value[index]
      if (item?.timer) clearTimeout(item.timer)
      items.value.splice(index, 1)
    }
  }

  function show(opts: ShowToastOptions | string): string {
    const options: ShowToastOptions = typeof opts === 'string' ? { text: opts } : opts
    const id = `toast-${nextId++}-${Date.now()}`
    const tone = options.tone ?? 'info'
    const defaultDuration = tone === 'danger' || tone === 'warning' ? 6000 : 4000
    const duration = options.duration ?? defaultDuration

    const item: ToastItem = {
      id,
      tone,
      text: options.text,
      description: options.description,
      duration,
    }

    if (duration > 0) {
      item.timer = setTimeout(() => {
        dismiss(id)
      }, duration)
    }

    items.value.push(item)
    return id
  }

  function success(text: string, description?: string, duration?: number) {
    return show({ tone: 'success', text, description, duration })
  }

  function danger(text: string, description?: string, duration?: number) {
    return show({ tone: 'danger', text, description, duration })
  }

  function warning(text: string, description?: string, duration?: number) {
    return show({ tone: 'warning', text, description, duration })
  }

  function info(text: string, description?: string, duration?: number) {
    return show({ tone: 'info', text, description, duration })
  }

  function clear() {
    for (const item of items.value) {
      if (item.timer) clearTimeout(item.timer)
    }
    items.value = []
  }

  return {
    items,
    show,
    success,
    danger,
    warning,
    info,
    dismiss,
    clear,
  }
})
