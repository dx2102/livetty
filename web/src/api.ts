import { Upload, type DetailedError } from 'tus-js-client'

export interface FileEntry {
  name: string
  is_dir: boolean
  size: number
  mtime_ms: number
}

/**
 * Files above this go through resumable tus uploads in chunks of this size.
 * Each request stays well under Cloudflare's 100 MB request body limit.
 */
export const CHUNK = 32 * 1024 * 1024

/** Pull our JSON {"error": ...} out of a tus error, else keep its message. */
function tusMessage(e: Error | DetailedError): string {
  const body = (e as DetailedError).originalResponse?.getBody()
  try {
    const msg = JSON.parse(body || '').error
    if (msg) return msg
  } catch {}
  return e.message
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
  /**
   * Upload a big file in CHUNK sized pieces, retrying through network drops.
   * Dropping the same file into the same folder again resumes where it
   * stopped, since tus-js-client remembers the upload URL in localStorage.
   */
  uploadResumable: (path: string, file: File, onProgress: (sent: number, total: number) => void) =>
    new Promise<void>((resolve, reject) => {
      const up = new Upload(file, {
        endpoint: '/api/tus',
        chunkSize: CHUNK,
        retryDelays: [0, 1000, 3000, 5000, 10000, 20000, 30000],
        metadata: { path },
        // The default fingerprint ignores where the file goes. Include it, so
        // the same file dropped into another folder is a separate upload.
        fingerprint: async (f: File) => ['livetty', path, f.size, f.lastModified].join(':'),
        removeFingerprintOnSuccess: true,
        // Retry network errors, 5xx, 409 (offset mismatch, tus resyncs with
        // HEAD) and 423 (locked). Other 4xx, like "file already exists" at
        // creation, will not get better by retrying.
        onShouldRetry: (e) => {
          const s = e.originalResponse?.getStatus() ?? 0
          if (e.originalRequest?.getMethod() === 'POST' && s >= 400 && s < 500) return false
          return s === 0 || s >= 500 || s === 409 || s === 423
        },
        onProgress,
        onSuccess: () => resolve(),
        onError: (e) => reject(new Error(tusMessage(e))),
      })
      up.findPreviousUploads().then((prev) => {
        if (prev.length) up.resumeFromPreviousUpload(prev[0])
        up.start()
      }, reject)
    }),
}
