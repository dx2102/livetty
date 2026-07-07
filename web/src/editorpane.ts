import { EditorState, type Extension } from '@codemirror/state'
import { EditorView, keymap } from '@codemirror/view'
import { indentWithTab } from '@codemirror/commands'
import { basicSetup } from 'codemirror'
import { javascript } from '@codemirror/lang-javascript'
import { rust } from '@codemirror/lang-rust'
import { python } from '@codemirror/lang-python'
import { json } from '@codemirror/lang-json'
import { markdown } from '@codemirror/lang-markdown'
import { html } from '@codemirror/lang-html'
import { css } from '@codemirror/lang-css'
import { yaml } from '@codemirror/lang-yaml'
import { cpp } from '@codemirror/lang-cpp'
import { sql } from '@codemirror/lang-sql'
import { api } from './api'

function langFor(path: string): Extension | null {
  const name = path.split('/').pop() || ''
  const lc = name.toLowerCase()
  const ext = lc.includes('.') ? lc.split('.').pop()! : ''
  switch (ext) {
    case 'js': case 'mjs': case 'cjs': case 'jsx':
      return javascript({ jsx: ext === 'jsx' })
    case 'ts':
      return javascript({ typescript: true })
    case 'tsx':
      return javascript({ typescript: true, jsx: true })
    case 'rs': return rust()
    case 'py': case 'pyi': return python()
    case 'json': case 'jsonc': return json()
    case 'md': case 'markdown': return markdown()
    case 'html': case 'htm': return html()
    case 'css': case 'scss': case 'less': return css()
    case 'yml': case 'yaml': return yaml()
    case 'c': case 'h': case 'cc': case 'cpp': case 'hpp': case 'cxx':
      return cpp()
    case 'sql': return sql()
  }
  // filename-based
  if (lc === 'dockerfile' || lc === 'makefile') return null
  return null
}

const editorTheme = EditorView.theme({
  '&': { fontSize: '20px', height: '100%' },
  '.cm-scroller': {
    fontFamily:
      "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New', monospace",
    lineHeight: '1.5',
  },
  '.cm-content': { padding: '8px 0' },
  '.cm-gutters': {
    backgroundColor: '#fafafa',
    borderRight: '1px solid #e5e7eb',
    paddingLeft: '0',
  },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 4px 0 4px' },
  '.cm-foldGutter .cm-gutterElement': { padding: '0' },
})

export class EditorPane {
  readonly path: string
  readonly el: HTMLElement
  private view: EditorView
  private mtime: number
  dirty = false
  onDirtyChange: (dirty: boolean) => void = () => {}

  private constructor(path: string, content: string, mtime: number) {
    this.path = path
    this.mtime = mtime
    this.el = document.createElement('div')
    this.el.className = 'w-full h-full overflow-hidden bg-white'

    const saveKey = keymap.of([
      {
        key: 'Mod-s',
        preventDefault: true,
        run: () => {
          void this.save()
          return true
        },
      },
    ])

    const changeListener = EditorView.updateListener.of((u) => {
      if (u.docChanged && !this.dirty) {
        this.dirty = true
        this.onDirtyChange(true)
      }
    })

    const exts: Extension[] = [
      basicSetup,
      keymap.of([indentWithTab]),
      saveKey,
      changeListener,
      editorTheme,
      EditorView.lineWrapping,
    ]
    const lang = langFor(path)
    if (lang) exts.push(lang)

    this.view = new EditorView({
      state: EditorState.create({ doc: content, extensions: exts }),
      parent: this.el,
    })
  }

  static async open(path: string): Promise<EditorPane> {
    const { content, mtime_ms } = await api.readFile(path)
    return new EditorPane(path, content, mtime_ms)
  }

  async save(): Promise<void> {
    const content = this.view.state.doc.toString()
    try {
      const res = await api.writeFile(this.path, content, this.mtime)
      this.mtime = res.mtime_ms
      this.dirty = false
      this.onDirtyChange(false)
    } catch (e: any) {
      if (e.status === 409) {
        const overwrite = confirm(
          `${this.path}\n\nThe file was modified externally (e.g. by an editor in the terminal).\n\n[OK] = overwrite disk with current editor content\n[Cancel] = do nothing, keep your edits in the editor, leave the file on disk untouched`,
        )
        if (overwrite) {
          const res = await api.writeFile(this.path, content)
          this.mtime = res.mtime_ms
          this.dirty = false
          this.onDirtyChange(false)
        }
        // Cancel: intentionally do nothing. Editor stays dirty; disk untouched.
        // The user can then diff / copy their edits out manually.
      } else {
        alert(`Save failed: ${e.message}`)
      }
    }
  }

  show() {
    this.view.focus()
  }

  hide() {}

  dispose() {
    this.view.destroy()
    this.el.remove()
  }
}
