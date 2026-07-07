import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebglAddon } from '@xterm/addon-webgl'
import { SearchAddon, type ISearchOptions } from '@xterm/addon-search'
import '@xterm/xterm/css/xterm.css'
import type { WsClient } from './wsclient'

// Soft amber match highlights, no visible border. Active match is one shade
// deeper so you can still tell it apart from the rest.
const SEARCH_OPTS: ISearchOptions = {
  decorations: {
    matchBackground: '#fef3c7',
    matchBorder: 'transparent',
    matchOverviewRuler: '#fcd34d',
    activeMatchBackground: '#fcd34d',
    activeMatchBorder: 'transparent',
    activeMatchColorOverviewRuler: '#f59e0b',
  },
}

// Match ttyd default: white bg, black fg, xterm.js built-in Tango 16-color palette.
// cursor/selection kept explicit since xterm.js defaults would be white on white.
const LIGHT_THEME = {
  background: '#ffffff',
  foreground: '#000000',
  cursor: '#000000',
  cursorAccent: '#ffffff',
  selectionBackground: '#b6d7ff',
  black: '#2e3436',
  red: '#cc0000',
  green: '#4e9a06',
  yellow: '#c4a000',
  blue: '#3465a4',
  magenta: '#75507b',
  cyan: '#06989a',
  white: '#d3d7cf',
  brightBlack: '#555753',
  brightRed: '#ef2929',
  brightGreen: '#8ae234',
  brightYellow: '#fce94f',
  brightBlue: '#729fcf',
  brightMagenta: '#ad7fa8',
  brightCyan: '#34e2e2',
  brightWhite: '#eeeeec',
}

export class TermPane {
  readonly id: number
  readonly el: HTMLElement
  private term: Terminal
  private fit: FitAddon
  private webgl: WebglAddon | null = null
  private search: SearchAddon
  private ws: WsClient
  private overlay: HTMLElement
  private holder: HTMLElement
  private opened = false
  subId = 0
  exited = false
  private visible = false
  // Search bar (lazy-built on first use)
  private searchBar: HTMLElement | null = null
  private searchInput: HTMLInputElement | null = null
  private searchCounter: HTMLElement | null = null

  constructor(id: number, ws: WsClient) {
    this.id = id
    this.ws = ws
    this.el = document.createElement('div')
    this.el.className = 'relative w-full h-full'
    this.holder = document.createElement('div')
    this.holder.className = 'absolute inset-0 pl-2 pt-2 bg-white'
    this.el.appendChild(this.holder)

    this.overlay = document.createElement('div')
    this.overlay.className =
      'absolute inset-0 hidden items-center justify-center bg-white/70 text-gray-500 text-base z-10'
    this.el.appendChild(this.overlay)

    this.term = new Terminal({
      scrollback: 10000, // local scrollback: zero-latency scrolling
      fontSize: 20,
      fontFamily:
        "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New', monospace",
      theme: LIGHT_THEME,
      cursorBlink: true,
      allowProposedApi: true,
    })
    this.fit = new FitAddon()
    this.term.loadAddon(this.fit)
    this.search = new SearchAddon()
    this.term.loadAddon(this.search)
    this.search.onDidChangeResults((r) => {
      if (!this.searchCounter) return
      if (!r || r.resultCount === 0) {
        this.searchCounter.textContent = this.searchInput?.value ? 'no match' : ''
      } else {
        this.searchCounter.textContent = `${r.resultIndex + 1} / ${r.resultCount}`
      }
    })
    // Intercept Ctrl/Cmd+F before xterm forwards it to the pty.
    this.term.attachCustomKeyEventHandler((ev) => {
      if (
        ev.type === 'keydown' &&
        (ev.ctrlKey || ev.metaKey) &&
        !ev.altKey &&
        ev.key.toLowerCase() === 'f'
      ) {
        ev.preventDefault()
        this.openSearch()
        return false
      }
      return true
    })
    // NOTE: DO NOT call this.term.open() here, el is still detached.
    // xterm requires its container be in the DOM & visible; otherwise
    // _core._store never initializes and every future op throws.

    this.term.onData((d) => this.ws.sendInput(this.id, d))
    this.term.onBinary((d) => {
      const bytes = new Uint8Array(d.length)
      for (let i = 0; i < d.length; i++) bytes[i] = d.charCodeAt(i) & 0xff
      this.ws.sendInput(this.id, bytes)
    })
    this.term.onResize(({ rows, cols }) => {
      this.ws.sendOp({ op: 'resize', id: this.id, rows, cols })
    })

    new ResizeObserver(() => {
      if (this.visible && this.opened) this.fitWithPadding()
    }).observe(this.holder)
  }

  private ensureOpen() {
    if (this.opened) return
    // holder must be attached & visible before term.open (xterm requirement)
    this.term.open(this.holder)
    this.opened = true
  }

  /**
   * fit, but shave one column so the last character never touches the right edge
   * and xterm's viewport (with its scrollbar) still spans the full holder width.
   */
  private fitWithPadding() {
    if (!this.opened) return
    const dims = this.fit.proposeDimensions()
    if (!dims || dims.cols < 3 || dims.rows < 2) return
    const cols = dims.cols - 1
    const rows = dims.rows
    if (this.term.cols !== cols || this.term.rows !== rows) {
      this.term.resize(cols, rows)
    }
  }

  attach() {
    this.ws.sendOp({ op: 'attach', id: this.id })
  }

  /** Server 'attached' event: clear local state, prepare to receive the snapshot replay. */
  onAttached(sub: number, exited: boolean) {
    this.subId = sub
    if (this.opened) this.term.reset()
    this.setExited(exited)
    // After attach, align PTY size with the actual viewport
    if (this.visible && this.opened) {
      this.fitWithPadding()
      this.ws.sendOp({
        op: 'resize',
        id: this.id,
        rows: this.term.rows,
        cols: this.term.cols,
      })
    }
  }

  write(data: Uint8Array) {
    this.term.write(data)
  }

  setExited(exited: boolean) {
    this.exited = exited
    if (exited) {
      this.overlay.textContent = 'Process exited, close the tab or remove it from the sidebar'
      this.overlay.classList.remove('hidden')
      this.overlay.classList.add('flex')
    } else {
      this.overlay.classList.add('hidden')
      this.overlay.classList.remove('flex')
    }
  }

  /** Only visible terminals mount WebGL (browsers cap the number of WebGL contexts). */
  show() {
    this.visible = true
    this.ensureOpen()
    this.fitWithPadding()
    if (!this.webgl) {
      try {
        this.webgl = new WebglAddon()
        this.webgl.onContextLoss(() => {
          try {
            this.webgl?.dispose()
          } catch {}
          this.webgl = null
        })
        this.term.loadAddon(this.webgl)
      } catch {
        this.webgl = null
      }
    }
    this.term.focus()
  }

  hide() {
    this.visible = false
    if (this.webgl) {
      try {
        this.webgl.dispose()
      } catch {}
      this.webgl = null
    }
  }

  detach() {
    if (this.subId) this.ws.sendOp({ op: 'detach', id: this.id, sub: this.subId })
  }

  // ---------- Search ----------

  private openSearch() {
    if (!this.searchBar) this.buildSearchBar()
    // Use inline style: `.hidden`(display:none) and `.flex`(display:flex) both
    // sit on the bar's class list, and utility-CSS ordering makes them fight.
    this.searchBar!.style.display = ''
    this.searchInput!.focus()
    this.searchInput!.select()
  }

  private closeSearch() {
    if (this.searchBar) this.searchBar.style.display = 'none'
    try {
      this.search.clearDecorations()
    } catch {}
    this.term.focus()
  }

  private buildSearchBar() {
    const bar = document.createElement('div')
    bar.className =
      'absolute top-2 right-4 z-20 bg-white border border-gray-300 rounded shadow flex items-center gap-1 px-1.5 py-1 text-base'
    bar.style.display = 'none'
    const input = document.createElement('input')
    input.type = 'text'
    input.placeholder = 'Find'
    input.className =
      'border border-gray-200 rounded px-2 py-0.5 text-base w-48 focus:outline-none focus:border-blue-400'
    const counter = document.createElement('span')
    counter.className = 'text-base text-gray-400 tabular-nums min-w-16 text-right px-1'
    const mkBtn = (label: string, title: string, fn: () => void) => {
      const b = document.createElement('button')
      b.textContent = label
      b.title = title
      b.className = 'w-6 h-6 rounded text-gray-500 hover:bg-gray-100 leading-none'
      // mousedown for snap response; preventDefault keeps the input focused.
      b.onmousedown = (e) => {
        if (e.button !== 0) return
        e.preventDefault()
        fn()
      }
      return b
    }
    const findNext = () => {
      const q = input.value
      if (q) this.search.findNext(q, SEARCH_OPTS)
    }
    const findPrev = () => {
      const q = input.value
      if (q) this.search.findPrevious(q, SEARCH_OPTS)
    }
    input.onkeydown = (e) => {
      if (e.key === 'Enter') {
        e.preventDefault()
        if (e.shiftKey) findPrev()
        else findNext()
      } else if (e.key === 'Escape') {
        e.preventDefault()
        this.closeSearch()
      }
    }
    // Live search as the user types (empty string → wipe decorations).
    input.oninput = () => {
      const q = input.value
      if (q) this.search.findNext(q, SEARCH_OPTS)
      else {
        try {
          this.search.clearDecorations()
        } catch {}
        counter.textContent = ''
      }
    }
    bar.append(
      input,
      counter,
      mkBtn('↑', 'Previous (Shift+Enter)', findPrev),
      mkBtn('↓', 'Next (Enter)', findNext),
      mkBtn('×', 'Close (Esc)', () => this.closeSearch()),
    )
    this.el.appendChild(bar)
    this.searchBar = bar
    this.searchInput = input
    this.searchCounter = counter
  }

  dispose() {
    this.detach()
    if (this.webgl) {
      try {
        this.webgl.dispose()
      } catch {}
      this.webgl = null
    }
    try {
      this.search.dispose()
    } catch {}
    try {
      this.term.dispose()
    } catch {}
    this.el.remove()
  }
}
