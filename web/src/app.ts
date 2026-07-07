import { api, type FileEntry } from './api'
import { WsClient, type TermInfo, type WsEvent } from './wsclient'
import { TermPane } from './termpane'
import { EditorPane } from './editorpane'

type PaneKind = 'term' | 'file' | 'settings'

interface Tab {
  key: string
  kind: PaneKind
  title: string
  btn: HTMLElement
  titleEl: HTMLElement
  pane: TermPane | EditorPane | HTMLElement
}

function fmtMtime(ms: number): string {
  if (!ms) return ''
  const diff = Date.now() - ms
  if (diff < 0) return 'now'
  const s = Math.floor(diff / 1000)
  if (s < 5) return 'now'
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  const d = Math.floor(h / 24)
  if (d < 30) return `${d}d ago`
  const mo = Math.floor(d / 30)
  if (mo < 12) return `${mo}mo ago`
  return `${Math.floor(d / 365)}y ago`
}

interface CtxItem {
  label: string
  onclick: () => void
  danger?: boolean
}

function showContextMenu(x: number, y: number, items: CtxItem[]) {
  document.querySelectorAll('[data-ctxmenu]').forEach((e) => e.remove())
  const menu = document.createElement('div')
  menu.dataset.ctxmenu = '1'
  menu.className =
    'fixed bg-white border border-gray-200 rounded shadow-lg py-1 z-50 min-w-32 text-base'
  menu.style.left = `${x}px`
  menu.style.top = `${y}px`
  for (const it of items) {
    const el = document.createElement('button')
    el.textContent = it.label
    el.className =
      'w-full text-left px-3 py-1 ' +
      (it.danger ? 'text-red-600 hover:bg-red-50' : 'hover:bg-gray-100')
    el.onclick = () => {
      menu.remove()
      it.onclick()
    }
    menu.appendChild(el)
  }
  document.body.appendChild(menu)
  // clamp inside viewport
  const rect = menu.getBoundingClientRect()
  if (rect.right > innerWidth) menu.style.left = `${x - rect.width}px`
  if (rect.bottom > innerHeight) menu.style.top = `${y - rect.height}px`

  const close = (e: Event) => {
    if (e instanceof KeyboardEvent) {
      if (e.key !== 'Escape') return
    } else if (e instanceof MouseEvent && menu.contains(e.target as Node)) {
      return
    }
    menu.remove()
    document.removeEventListener('mousedown', close, true)
    document.removeEventListener('contextmenu', close, true)
    document.removeEventListener('keydown', close, true)
  }
  setTimeout(() => {
    document.addEventListener('mousedown', close, true)
    document.addEventListener('contextmenu', close, true)
    document.addEventListener('keydown', close, true)
  }, 0)
}

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag)
  el.className = cls
  if (text !== undefined) el.textContent = text
  return el
}

export class App {
  private ws = new WsClient()
  private terminals = new Map<number, TermInfo>()
  private tabs = new Map<string, Tab>()
  private activeKey: string | null = null
  private home: string
  private hostname: string
  private cwd: string

  // DOM
  private root!: HTMLElement
  private tabBar!: HTMLElement
  private plusBtn!: HTMLElement
  private statusDot!: HTMLElement
  private paneArea!: HTMLElement
  private sideToolbar!: HTMLElement
  private sideContent!: HTMLElement
  private sideEl!: HTMLElement
  private gripEl!: HTMLElement
  private sideMode: 'files' | 'terms' = 'files'
  private sideOpen = true
  private sideWidth = Math.max(180, Math.min(800, Number(localStorage.getItem('sideWidth')) || 360))
  private activityBtns: Record<string, HTMLElement> = {}

  constructor(home: string, hostname: string) {
    this.home = home
    this.hostname = hostname
    this.cwd = home
  }

  mount(root: HTMLElement) {
    ;(window as any).__remote = this // hook for E2E tests
    this.root = root
    root.innerHTML = ''
    root.className = 'flex flex-col h-full bg-white text-gray-800'

    // ---- Top bar ----
    const top = h('div', 'flex items-center h-10 border-b border-gray-200 px-2 gap-1 shrink-0')
    const brand = h('div', 'text-base font-semibold text-gray-700 px-1 select-none', 'remote')
    this.statusDot = h('div', 'w-2 h-2 rounded-full bg-yellow-400 shrink-0')
    this.statusDot.title = 'Connecting…'
    this.tabBar = h('div', 'flex items-center gap-0.5 flex-1 overflow-x-auto min-w-0 h-full')
    // JupyterLab-style "＋" launcher: lives INSIDE the tab bar, always the last
    // child, so it sits flush against the last tab (not the far right of the
    // screen). New tabs get insertBefore(plusBtn) to keep it as the tail.
    this.plusBtn = h(
      'button',
      'shrink-0 w-8 h-8 flex items-center justify-center text-xl text-gray-500 hover:bg-gray-100 rounded',
      '＋',
    )
    this.plusBtn.title = 'New…'
    this.plusBtn.onmousedown = (e) => {
      if (e.button !== 0) return
      const r = this.plusBtn.getBoundingClientRect()
      showContextMenu(r.left, r.bottom, [
        { label: 'New terminal', onclick: () => this.createTerm() },
        { label: 'New file', onclick: () => void this.newFile() },
      ])
    }
    this.tabBar.appendChild(this.plusBtn)
    top.append(brand, this.statusDot, this.tabBar)

    // ---- Main area: [activity bar | sidebar | grip | pane area] ----
    const main = h('div', 'flex flex-1 min-h-0')

    // Activity bar: narrow leftmost strip with 📁 / ⚡ icons
    const activity = h(
      'div',
      'w-11 shrink-0 flex flex-col items-center py-1 gap-1 border-r border-gray-200 bg-gray-100',
    )
    const mkAct = (mode: 'files' | 'terms', emoji: string, title: string) => {
      const b = h(
        'button',
        'w-9 h-9 flex items-center justify-center rounded text-xl hover:bg-gray-200',
      )
      b.textContent = emoji
      b.title = title
      b.onmousedown = (e) => {
        if (e.button !== 0) return
        this.toggleSide(mode)
      }
      this.activityBtns[mode] = b
      return b
    }
    activity.append(mkAct('files', '📁', 'Files'), mkAct('terms', '⚡', 'Terminals'))

    // Sidebar content (collapsible + resizable).
    // Layout is flex-col: a fixed toolbar slot on top, then the scrollable list.
    // Keeping the toolbar OUT of the scroll container avoids sticky/z-index
    // stacking headaches, it's literally a sibling of the scroll box.
    this.sideEl = h('div', 'shrink-0 border-r border-gray-200 flex flex-col overflow-hidden')
    this.sideEl.style.width = `${this.sideWidth}px`
    this.sideToolbar = h('div', 'shrink-0')
    this.sideContent = h('div', 'flex-1 overflow-y-auto min-h-0')
    this.sideEl.append(this.sideToolbar, this.sideContent)

    // Resize grip (only visible while sidebar is open)
    this.gripEl = h(
      'div',
      'w-1 shrink-0 cursor-col-resize hover:bg-blue-400 active:bg-blue-500 bg-transparent',
    )
    this.bindResize()

    this.paneArea = h('div', 'flex-1 relative min-w-0 bg-white')
    const empty = h(
      'div',
      'absolute inset-0 flex items-center justify-center text-gray-300 text-base select-none',
      'Open a file or terminal from the sidebar',
    )
    empty.dataset.empty = '1'
    this.paneArea.appendChild(empty)

    main.append(activity, this.sideEl, this.gripEl, this.paneArea)
    root.append(top, main)
    this.updateActivityBtns()

    // ---- WS ----
    this.ws.onEvent = (ev) => this.handleEvent(ev)
    this.ws.onBytes = (id, data) => {
      const tab = this.tabs.get(`term:${id}`)
      if (tab) (tab.pane as TermPane).write(data)
    }
    this.ws.onOpen = () => {
      this.setStatus(true)
      // After reconnect: re-attach every open terminal (snapshot fully replays the screen, self-healing)
      for (const tab of this.tabs.values()) {
        if (tab.kind === 'term') (tab.pane as TermPane).attach()
      }
    }
    this.ws.onDown = () => this.setStatus(false)
    this.ws.connect()

    this.renderSide()
    void this.loadDir(this.cwd)

    window.addEventListener('beforeunload', (e) => {
      for (const t of this.tabs.values()) {
        if (t.kind === 'file' && (t.pane as EditorPane).dirty) {
          e.preventDefault()
          return
        }
      }
    })
  }

  private setStatus(ok: boolean) {
    this.statusDot.className = `w-2 h-2 rounded-full shrink-0 ${ok ? 'bg-green-500' : 'bg-yellow-400'}`
    this.statusDot.title = ok ? 'Connected' : 'Reconnecting…'
  }

  // ---------- WS events ----------

  private handleEvent(ev: WsEvent) {
    switch (ev.ev) {
      case 'hello':
        this.terminals.clear()
        for (const t of ev.terminals) this.terminals.set(t.id, t)
        this.renderSide()
        break
      case 'create_ok':
        this.terminals.set(ev.term.id, ev.term)
        this.renderSide()
        this.openTerm(ev.term.id)
        break
      case 'created':
        this.terminals.set(ev.term.id, ev.term)
        this.renderSide()
        break
      case 'attached': {
        const tab = this.tabs.get(`term:${ev.id}`)
        if (tab) (tab.pane as TermPane).onAttached(ev.sub, ev.exited)
        break
      }
      case 'removed': {
        this.terminals.delete(ev.id)
        this.renderSide()
        const key = `term:${ev.id}`
        if (this.tabs.has(key)) this.closeTab(key, true)
        break
      }
      case 'exited': {
        const t = this.terminals.get(ev.id)
        if (t) t.exited = true
        this.renderSide()
        const tab = this.tabs.get(`term:${ev.id}`)
        if (tab) (tab.pane as TermPane).setExited(true)
        break
      }
      case 'title': {
        const t = this.terminals.get(ev.id)
        if (t) t.title = ev.title
        this.renderSide()
        const tab = this.tabs.get(`term:${ev.id}`)
        if (tab) this.setTabTitle(tab, ev.title || `Terminal ${ev.id}`)
        break
      }
      case 'resized':
        break
      case 'error':
        this.toast(ev.msg)
        break
    }
  }

  // ---------- Tabs ----------

  private addTab(key: string, kind: PaneKind, title: string, pane: Tab['pane']) {
    const btn = h(
      'div',
      'group relative flex items-center gap-1 h-full w-52 shrink-0 pl-2 pr-7 text-base cursor-pointer border-b-2 border-transparent hover:bg-gray-100 select-none whitespace-nowrap',
    )
    const kindIcon = h('span', 'shrink-0 select-none', kind === 'term' ? '⚡' : '📄')
    const titleEl = h('span', 'flex-1 min-w-0 truncate', title)
    const close = h(
      'button',
      'absolute right-1 top-1/2 -translate-y-1/2 invisible group-hover:visible text-gray-400 hover:text-gray-700 hover:bg-gray-200 rounded w-5 h-5 leading-none flex items-center justify-center',
      '×',
    )
    close.onclick = (e) => {
      e.stopPropagation()
      this.closeTab(key)
    }
    // Keep the × in the "click to close" B group, but stop its mousedown from
    // bubbling, otherwise the parent tab's mousedown would activate the tab
    // right before the close click fires.
    close.onmousedown = (e) => e.stopPropagation()
    btn.append(kindIcon, titleEl, close)
    btn.onmousedown = (e) => {
      if (e.button !== 0) return
      this.activate(key)
    }
    // Insert before the trailing ＋ so it stays as the last child of tabBar.
    this.tabBar.insertBefore(btn, this.plusBtn)

    const tab: Tab = { key, kind, title, btn, titleEl, pane }
    this.tabs.set(key, tab)
    const el = pane instanceof HTMLElement ? pane : pane.el
    el.classList.add('absolute', 'inset-0')
    el.style.display = 'none'
    this.paneArea.appendChild(el)
    return tab
  }

  private setTabTitle(tab: Tab, title: string, dirty = false) {
    tab.title = title
    tab.titleEl.textContent = (dirty ? '● ' : '') + title
    tab.titleEl.title = title
  }

  private activate(key: string) {
    if (this.activeKey === key) return
    const prev = this.activeKey ? this.tabs.get(this.activeKey) : null
    if (prev) {
      this.paneEl(prev).style.display = 'none'
      if (!(prev.pane instanceof HTMLElement)) {
        try {
          prev.pane.hide()
        } catch (e) {
          console.warn('pane.hide failed:', e)
        }
      }
      prev.btn.classList.remove('border-blue-500', 'bg-gray-100')
    }
    const tab = this.tabs.get(key)
    if (!tab) return
    this.activeKey = key
    this.paneEl(tab).style.display = ''
    tab.btn.classList.add('border-blue-500', 'bg-gray-100')
    if (!(tab.pane instanceof HTMLElement)) {
      try {
        tab.pane.show()
      } catch (e) {
        console.warn('pane.show failed:', e)
      }
    }
    // Keep sidebar's active-terminal highlight in sync with the top tab bar.
    if (this.sideOpen && this.sideMode === 'terms') this.renderSide()
  }

  private paneEl(tab: Tab): HTMLElement {
    return tab.pane instanceof HTMLElement ? tab.pane : tab.pane.el
  }

  private closeTab(key: string, force = false) {
    const tab = this.tabs.get(key)
    if (!tab) return
    if (!force && tab.kind === 'file' && (tab.pane as EditorPane).dirty) {
      if (!confirm(`${tab.title} has unsaved changes. Close anyway?`)) return
    }
    if (tab.pane instanceof HTMLElement) tab.pane.remove()
    else tab.pane.dispose()
    tab.btn.remove()
    this.tabs.delete(key)
    if (this.activeKey === key) {
      this.activeKey = null
      const rest = [...this.tabs.keys()]
      if (rest.length) this.activate(rest[rest.length - 1])
      else if (this.sideOpen && this.sideMode === 'terms') this.renderSide()
    }
  }

  // ---------- Terminals ----------

  private openTerm(id: number) {
    const key = `term:${id}`
    if (this.tabs.has(key)) {
      this.activate(key)
      return
    }
    const pane = new TermPane(id, this.ws)
    const info = this.terminals.get(id)
    this.addTab(key, 'term', info?.title || `Terminal ${id}`, pane)
    this.activate(key)
    pane.attach()
  }

  private createTerm() {
    this.ws.sendOp({ op: 'create', cwd: this.cwd, rows: 24, cols: 80 })
  }

  private async newFile() {
    const name = prompt('New file name:')
    if (!name) return
    const dest = `${this.cwd === '/' ? '' : this.cwd}/${name}`
    try {
      await api.fsOp({ op: 'touch', path: dest })
      await this.loadDir(this.cwd)
      await this.openFile(dest)
    } catch (e: any) {
      this.toast(e.message)
    }
  }

  // ---------- Files ----------

  private async openFile(path: string) {
    const key = `file:${path}`
    if (this.tabs.has(key)) {
      this.activate(key)
      return
    }
    try {
      const pane = await EditorPane.open(path)
      const name = path.split('/').pop() || path
      const tab = this.addTab(key, 'file', name, pane)
      pane.onDirtyChange = (d) => this.setTabTitle(tab, name, d)
      this.activate(key)
    } catch (e: any) {
      this.toast(`Failed to open: ${e.message}`)
    }
  }

  private async loadDir(path: string) {
    try {
      const res = await api.listDir(path)
      this.cwd = res.path
      this.renderSide(res.entries)
    } catch (e: any) {
      this.toast(`Failed to read directory: ${e.message}`)
    }
  }

  // ---------- Sidebar ----------

  private lastEntries: FileEntry[] = []

  private renderSide(entries?: FileEntry[]) {
    if (entries) this.lastEntries = entries
    this.sideToolbar.innerHTML = ''
    this.sideContent.innerHTML = ''
    if (!this.sideOpen) return
    if (this.sideMode === 'files') this.renderFiles()
    else this.renderTerms()
  }

  private toggleSide(mode: 'files' | 'terms') {
    if (this.sideOpen && this.sideMode === mode) {
      this.sideOpen = false
    } else {
      this.sideMode = mode
      this.sideOpen = true
    }
    this.applySideVisibility()
    this.updateActivityBtns()
    this.renderSide()
  }

  private applySideVisibility() {
    if (this.sideOpen) {
      this.sideEl.style.display = ''
      this.sideEl.style.width = `${this.sideWidth}px`
      this.gripEl.style.display = ''
    } else {
      this.sideEl.style.display = 'none'
      this.gripEl.style.display = 'none'
    }
  }

  private updateActivityBtns() {
    for (const [mode, btn] of Object.entries(this.activityBtns)) {
      const active = this.sideOpen && this.sideMode === mode
      btn.className =
        'w-9 h-9 flex items-center justify-center rounded text-xl ' +
        (active ? 'bg-white' : 'hover:bg-gray-200')
    }
  }

  private bindResize() {
    this.gripEl.addEventListener('mousedown', (e) => {
      e.preventDefault()
      const startX = e.clientX
      const startW = this.sideWidth
      document.body.style.cursor = 'col-resize'
      document.body.style.userSelect = 'none'
      const onMove = (ev: MouseEvent) => {
        const w = Math.max(180, Math.min(800, startW + (ev.clientX - startX)))
        this.sideWidth = w
        this.sideEl.style.width = `${w}px`
      }
      const onUp = () => {
        document.removeEventListener('mousemove', onMove)
        document.removeEventListener('mouseup', onUp)
        document.body.style.cursor = ''
        document.body.style.userSelect = ''
        localStorage.setItem('sideWidth', String(this.sideWidth))
      }
      document.addEventListener('mousemove', onMove)
      document.addEventListener('mouseup', onUp)
    })
  }

  private renderFiles() {
    const bar = h('div', 'flex items-center gap-1 p-1.5 border-b border-gray-100 bg-white')
    const pathInput = h('input', 'flex-1 min-w-0 text-base border border-gray-200 rounded px-2 py-1 focus:outline-none focus:border-blue-400') as HTMLInputElement
    pathInput.value = this.cwd
    pathInput.onkeydown = (e) => {
      if (e.key === 'Enter') void this.loadDir(pathInput.value.trim())
    }
    const mkBtn = (label: string, title: string, fn: () => void) => {
      const b = h('button', 'text-base text-gray-500 hover:bg-gray-100 rounded px-2 py-1 shrink-0', label)
      b.title = title
      b.onmousedown = (e) => {
        if (e.button !== 0) return
        fn()
      }
      return b
    }
    bar.append(
      pathInput,
      mkBtn('⟳', 'Refresh', () => void this.loadDir(this.cwd)),
      mkBtn('＋', 'New file', () => {
        const name = prompt('New file name:')
        if (!name) return
        void api
          .fsOp({ op: 'touch', path: `${this.cwd}/${name}` })
          .then(() => this.loadDir(this.cwd))
          .catch((e) => this.toast(e.message))
      }),
      mkBtn('▣', 'New folder', () => {
        const name = prompt('New folder name:')
        if (!name) return
        void api
          .fsOp({ op: 'mkdir', path: `${this.cwd}/${name}` })
          .then(() => this.loadDir(this.cwd))
          .catch((e) => this.toast(e.message))
      }),
    )
    this.sideToolbar.appendChild(bar)

    // ---- Column header ----
    const header = h('div', 'flex items-center gap-2 px-2 py-1 text-base text-gray-400 border-b border-gray-100 select-none')
    header.append(
      h('span', 'w-6 shrink-0', ''),
      h('span', 'flex-1 min-w-0', 'Name'),
      h('span', 'w-24 shrink-0 text-right', 'Modified'),
    )
    this.sideContent.appendChild(header)

    const list = h('div', 'py-1 relative min-h-32')
    this.bindDropUpload(list)
    // parent dir
    if (this.cwd !== '/') {
      const up = h('div', 'px-2 py-1 text-base text-gray-500 hover:bg-gray-100 cursor-pointer select-none', '.. /')
      up.onmousedown = (e) => {
        if (e.button !== 0) return
        const parent = this.cwd.replace(/\/[^/]+\/?$/, '') || '/'
        void this.loadDir(parent)
      }
      list.appendChild(up)
    }

    // Sort by mtime descending (newest first)
    const sorted = [...this.lastEntries].sort((a, b) => b.mtime_ms - a.mtime_ms)
    for (const ent of sorted) {
      const row = h(
        'div',
        'flex items-center gap-2 px-2 py-1 text-base hover:bg-gray-100 cursor-pointer select-none',
      )
      row.dataset.fileRow = '1'
      const icon = h('span', 'w-6 text-center shrink-0', ent.is_dir ? '📁' : '📄')
      const name = h('span', 'truncate flex-1 min-w-0', ent.name)
      const mtime = h('span', 'w-24 shrink-0 text-right text-gray-500 tabular-nums', fmtMtime(ent.mtime_ms))
      const full = `${this.cwd === '/' ? '' : this.cwd}/${ent.name}`
      row.onmousedown = (e) => {
        if (e.button !== 0) return
        if (ent.is_dir) void this.loadDir(full)
        else void this.openFile(full)
      }
      row.oncontextmenu = (e) => {
        e.preventDefault()
        const items: CtxItem[] = []
        if (!ent.is_dir) {
          items.push({
            label: 'Download',
            onclick: () => {
              const a = document.createElement('a')
              a.href = api.downloadUrl(full)
              a.download = ent.name
              document.body.appendChild(a)
              a.click()
              a.remove()
            },
          })
        }
        items.push({
          label: 'Rename',
          onclick: () => {
            const to = prompt('Rename to:', ent.name)
            if (!to || to === ent.name) return
            void api
              .fsOp({ op: 'rename', path: full, to: `${this.cwd}/${to}` })
              .then(() => this.loadDir(this.cwd))
              .catch((err) => this.toast(err.message))
          },
        })
        items.push({
          label: 'Delete',
          danger: true,
          onclick: () => {
            if (!confirm(`Delete ${ent.name}?${ent.is_dir ? ' (empty folders only)' : ''}`)) return
            void api
              .fsOp({ op: 'delete', path: full })
              .then(() => this.loadDir(this.cwd))
              .catch((err) => this.toast(err.message))
          },
        })
        showContextMenu(e.clientX, e.clientY, items)
      }
      row.append(icon, name, mtime)
      list.appendChild(row)
    }
    this.sideContent.appendChild(list)
  }

  private bindDropUpload(target: HTMLElement) {
    let depth = 0
    const overlay = h(
      'div',
      'absolute inset-0 hidden items-center justify-center bg-blue-50/80 border-2 border-dashed border-blue-400 text-blue-700 pointer-events-none select-none rounded z-10 text-base',
      'Drop to upload',
    )
    overlay.dataset.dropOverlay = '1'
    target.appendChild(overlay)
    const show = () => {
      overlay.classList.remove('hidden')
      overlay.classList.add('flex')
    }
    const hide = () => {
      depth = 0
      overlay.classList.add('hidden')
      overlay.classList.remove('flex')
    }
    target.addEventListener('dragenter', (e) => {
      if (!e.dataTransfer?.types.includes('Files')) return
      e.preventDefault()
      depth++
      show()
    })
    target.addEventListener('dragover', (e) => {
      if (!e.dataTransfer?.types.includes('Files')) return
      e.preventDefault()
      e.dataTransfer.dropEffect = 'copy'
    })
    target.addEventListener('dragleave', () => {
      depth--
      if (depth <= 0) hide()
    })
    target.addEventListener('drop', async (e) => {
      e.preventDefault()
      hide()
      const files = Array.from(e.dataTransfer?.files || [])
      if (!files.length) return
      let ok = 0
      let failed: string[] = []
      for (const f of files) {
        const dest = `${this.cwd === '/' ? '' : this.cwd}/${f.name}`
        try {
          await api.upload(dest, f)
          ok++
        } catch (err: any) {
          failed.push(`${f.name}: ${err.message}`)
        }
      }
      if (ok > 0) this.toast(`Uploaded ${ok} file${ok === 1 ? '' : 's'}`)
      for (const f of failed) this.toast(`Upload failed, ${f}`)
      void this.loadDir(this.cwd)
    })
  }

  private renderTerms() {
    const bar = h('div', 'p-1.5 border-b border-gray-100 bg-white')
    const create = h(
      'button',
      'w-full text-base border border-gray-200 rounded py-1 text-gray-600 hover:bg-gray-100',
      '＋ New terminal',
    )
    create.onmousedown = (e) => {
      if (e.button !== 0) return
      this.createTerm()
    }
    bar.appendChild(create)
    this.sideToolbar.appendChild(bar)

    const list = h('div', 'py-1')
    const terms = [...this.terminals.values()].sort((a, b) => a.id - b.id)
    if (!terms.length) {
      list.appendChild(h('div', 'px-3 py-2 text-base text-gray-400', 'No terminals'))
    }
    for (const t of terms) {
      const isActive = this.activeKey === `term:${t.id}`
      const row = h(
        'div',
        `group flex items-center gap-1.5 px-2 py-1.5 text-base cursor-pointer select-none ${
          isActive ? 'bg-gray-100' : 'hover:bg-gray-100'
        }`,
      )
      // id lives in the tooltip so CLI users can still look it up without
      // the sidebar wearing a "#3" tag on every row.
      row.title = `Terminal id: ${t.id}${t.title ? `, ${t.title}` : ''}`
      const dot = h(
        'span',
        `w-1.5 h-1.5 rounded-full shrink-0 ${t.exited ? 'bg-gray-300' : 'bg-green-500'}`,
      )
      const label = h('span', 'truncate flex-1 min-w-0', t.title || `Terminal ${t.id}`)
      const kill = h(
        'button',
        'hidden group-hover:block text-base text-gray-400 hover:text-red-600 px-0.5 shrink-0',
        '✕',
      )
      kill.title = 'Close (kill process)'
      kill.onclick = (e) => {
        e.stopPropagation()
        if (!confirm(`Close Terminal #${t.id}? The process will be killed.`)) return
        this.ws.sendOp({ op: 'kill', id: t.id })
      }
      // Same reason as tab-close: stop this from bubbling to the row's
      // mousedown, otherwise clicking × would openTerm first, then confirm.
      kill.onmousedown = (e) => e.stopPropagation()
      row.onmousedown = (e) => {
        if (e.button !== 0) return
        this.openTerm(t.id)
      }
      row.append(dot, label, kill)
      list.appendChild(row)
    }
    this.sideContent.appendChild(list)
  }

  // ---------- misc ----------

  private toast(msg: string) {
    const t = h(
      'div',
      'fixed bottom-4 right-4 bg-gray-800 text-white text-base rounded px-3 py-2 shadow-lg z-50 max-w-md',
      msg,
    )
    document.body.appendChild(t)
    setTimeout(() => t.remove(), 4000)
  }
}
