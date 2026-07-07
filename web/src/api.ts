export interface FileEntry {
  name: string
  is_dir: boolean
  size: number
  mtime_ms: number
}

async function jsonFetch(url: string, init?: RequestInit): Promise<any> {
  const res = await fetch(url, init)
  const body = await res.json().catch(() => ({}))
  if (!res.ok) {
    const err: any = new Error(body.error || `HTTP ${res.status}`)
    err.status = res.status
    err.body = body
    throw err
  }
  return body
}

export const api = {
  me: () => jsonFetch('/api/me'),
  login: (password: string) =>
    jsonFetch('/api/login', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ password }),
    }),
  logout: () => jsonFetch('/api/logout', { method: 'POST' }),
  listDir: (path: string): Promise<{ path: string; entries: FileEntry[] }> =>
    jsonFetch(`/api/files?path=${encodeURIComponent(path)}`),
  readFile: (path: string): Promise<{ content: string; mtime_ms: number }> =>
    jsonFetch(`/api/file?path=${encodeURIComponent(path)}`),
  writeFile: (path: string, content: string, expect_mtime_ms?: number) =>
    jsonFetch('/api/file', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ path, content, expect_mtime_ms }),
    }),
  fsOp: (op: { op: string; path: string; to?: string }) =>
    jsonFetch('/api/fs', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(op),
    }),
  downloadUrl: (path: string) => `/api/file/download?path=${encodeURIComponent(path)}`,
  upload: async (path: string, file: File | Blob) => {
    const res = await fetch(`/api/file/upload?path=${encodeURIComponent(path)}`, {
      method: 'POST',
      body: file,
    })
    const body = await res.json().catch(() => ({}))
    if (!res.ok) {
      const err: any = new Error(body.error || `HTTP ${res.status}`)
      err.status = res.status
      throw err
    }
    return body as { ok: boolean; size: number; mtime_ms: number }
  },
}
