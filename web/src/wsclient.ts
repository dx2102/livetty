// WS protocol: frame = [type: u8][term_id: u32 LE][payload]
// type 0 = raw terminal bytes; type 1 = JSON control/events

const FT_BYTES = 0
const FT_JSON = 1

export interface TermInfo {
  id: number
  title: string
  rows: number
  cols: number
  exited: boolean
}

export type WsEvent =
  | { ev: 'hello'; terminals: TermInfo[] }
  | { ev: 'create_ok'; term: TermInfo }
  | { ev: 'created'; term: TermInfo }
  | { ev: 'attached'; id: number; sub: number; exited: boolean }
  | { ev: 'removed'; id: number }
  | { ev: 'exited'; id: number }
  | { ev: 'title'; id: number; title: string }
  | { ev: 'resized'; id: number; rows: number; cols: number }
  | { ev: 'error'; msg: string }

type EventHandler = (ev: WsEvent) => void
type BytesHandler = (id: number, data: Uint8Array) => void

export class WsClient {
  private ws: WebSocket | null = null
  private backoff = 0
  private closed = false
  onEvent: EventHandler = () => {}
  onBytes: BytesHandler = () => {}
  /** Fires when the connection is (re)established: app uses it to re-attach all open terminals. */
  onOpen: () => void = () => {}
  onDown: () => void = () => {}

  connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws'
    const ws = new WebSocket(`${proto}://${location.host}/ws`)
    ws.binaryType = 'arraybuffer'
    this.ws = ws
    ws.onopen = () => {
      this.backoff = 0
      this.onOpen()
    }
    ws.onmessage = (e) => {
      if (!(e.data instanceof ArrayBuffer)) return
      const buf = new Uint8Array(e.data)
      if (buf.length < 5) return
      const ft = buf[0]
      const id = new DataView(e.data).getUint32(1, true)
      const payload = buf.subarray(5)
      if (ft === FT_BYTES) {
        this.onBytes(id, payload)
      } else if (ft === FT_JSON) {
        try {
          this.onEvent(JSON.parse(new TextDecoder().decode(payload)))
        } catch {}
      }
    }
    ws.onclose = () => {
      this.ws = null
      if (this.closed) return
      this.onDown()
      setTimeout(() => this.connect(), this.backoff)
      // 0 → 500 on first fail, then exponential doubling up to 5s
      this.backoff = this.backoff === 0 ? 500 : Math.min(this.backoff * 2, 5000)
    }
    ws.onerror = () => ws.close()
  }

  close() {
    this.closed = true
    this.ws?.close()
  }

  get connected() {
    return this.ws?.readyState === WebSocket.OPEN
  }

  private frame(ft: number, id: number, payload: Uint8Array) {
    const buf = new Uint8Array(5 + payload.length)
    buf[0] = ft
    new DataView(buf.buffer).setUint32(1, id, true)
    buf.set(payload, 5)
    return buf
  }

  sendInput(id: number, data: string | Uint8Array) {
    if (!this.connected) return
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data
    this.ws!.send(this.frame(FT_BYTES, id, bytes))
  }

  sendOp(op: Record<string, unknown>) {
    if (!this.connected) return
    const payload = new TextEncoder().encode(JSON.stringify(op))
    this.ws!.send(this.frame(FT_JSON, 0, payload))
  }
}
