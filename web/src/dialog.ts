/**
 * In-page replacement for window.confirm(). The native one can be silently
 * suppressed by the browser ("prevent this page from creating additional
 * dialogs" makes it return false without showing anything), so clicking a
 * close button would look like it did nothing.
 *
 * Enter = OK, Escape or clicking outside = Cancel. Keys are caught in the
 * capture phase so a focused xterm never sees the Enter.
 */
export function confirmDialog(msg: string, okLabel = 'OK'): Promise<boolean> {
  return new Promise((resolve) => {
    const prevFocus = document.activeElement as HTMLElement | null
    const backdrop = document.createElement('div')
    backdrop.className = 'fixed inset-0 bg-black/30 z-50 flex items-center justify-center'
    const box = document.createElement('div')
    box.className = 'bg-white rounded shadow-lg p-4 max-w-md w-[90vw] text-base'
    const text = document.createElement('div')
    text.className = 'whitespace-pre-wrap break-words mb-4'
    text.textContent = msg
    const row = document.createElement('div')
    row.className = 'flex justify-end gap-2'
    const cancel = document.createElement('button')
    cancel.className = 'px-3 py-1 rounded border border-gray-300 hover:bg-gray-100'
    cancel.textContent = 'Cancel'
    const ok = document.createElement('button')
    ok.className = 'px-3 py-1 rounded bg-blue-600 text-white hover:bg-blue-700'
    ok.textContent = okLabel
    row.append(cancel, ok)
    box.append(text, row)
    backdrop.appendChild(box)
    document.body.appendChild(backdrop)
    // Move focus off the terminal, otherwise its hidden textarea keeps it.
    ok.focus()

    const done = (result: boolean) => {
      window.removeEventListener('keydown', onKey, true)
      backdrop.remove()
      if (prevFocus?.isConnected) prevFocus.focus()
      resolve(result)
    }
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault()
      e.stopPropagation()
      // Tab toggles between the two buttons instead of leaving the dialog.
      if (e.key === 'Tab') (document.activeElement === ok ? cancel : ok).focus()
      else if (e.key === 'Enter') done(document.activeElement !== cancel)
      else if (e.key === 'Escape') done(false)
    }
    window.addEventListener('keydown', onKey, true)
    ok.onclick = () => done(true)
    cancel.onclick = () => done(false)
    backdrop.onmousedown = (e) => {
      if (e.target === backdrop) done(false)
    }
  })
}
