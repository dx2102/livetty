import { defineConfig } from 'vite'
import UnoCSS from 'unocss/vite'

export default defineConfig({
  plugins: [UnoCSS()],
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8737',
      '/ws': { target: 'ws://127.0.0.1:8737', ws: true },
    },
  },
  build: {
    chunkSizeWarningLimit: 8192,
  },
})
